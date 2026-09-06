from __future__ import annotations

import argparse
import copy
import hashlib
import importlib.util
import json
import os
import stat
import tempfile
import unittest
import zipfile
from pathlib import Path
from unittest import mock


ROOT = Path(__file__).parents[2]
SCRIPT = ROOT / "scripts/release/build-postrelease-public-truth.py"
PUBLISHED_SCRIPT = ROOT / "scripts/release/published-artifact-acceptance.py"


def load_module(name: str, path: Path):
    spec = importlib.util.spec_from_file_location(name, path)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


TRUTH = load_module("arc_postrelease_truth", SCRIPT)
PUBLISHED = load_module("arc_published_acceptance", PUBLISHED_SCRIPT)


def write_json(path: Path, value: object, *, canonical: bool = True) -> bytes:
    raw = (
        TRUTH.canonical_json(value)
        if canonical
        else (json.dumps(value, indent=2) + "\n").encode("utf-8")
    )
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(raw)
    return raw


def digest(raw: bytes) -> str:
    return hashlib.sha256(raw).hexdigest()


class Fixture:
    def __init__(self, root: Path) -> None:
        self.root = root
        self.source_sha = "a" * 40
        self.frontend_sha = "b" * 40
        self.release_run_id = 10
        self.release_run_attempt = 1
        self.acceptance_run_id = 14
        self.acceptance_run_attempt = 1
        self.readme = root / "README.md"
        self.readme.write_text(
            "# ARC\n\n"
            + TRUTH.BEGIN_MARKER
            + "\n> **Source-freeze snapshot:** not live.\n"
            + TRUTH.END_MARKER
            + "\n\n## The claim\nkept verbatim\n",
            encoding="utf-8",
        )

        self.manifest = {
            "mode": "production",
            "provenance": {"source_main_commit": self.source_sha},
            "checks": {
                "reward": {
                    "mode": "receipt",
                    "expected_reward_base": 2_500_000_000,
                    "expected_worker": "0x" + "c" * 64,
                }
            },
        }
        self.manifest_path = root / "manifest.json"
        manifest_raw = write_json(self.manifest_path, self.manifest)
        self.manifest_path.chmod(0o400)
        self.manifest_sidecar = root / "manifest.json.sha256"
        self.manifest_sidecar.write_text(
            f"{digest(manifest_raw)}  manifest.json\n", encoding="ascii"
        )
        self.manifest_sidecar.chmod(0o400)
        self.reward = {
            "schema": TRUTH.REWARD_SCHEMA,
            "rollout_sha256": digest(manifest_raw),
            "earnings_baseline": {
                "worker": "0x" + "c" * 64,
                "confirmed_receipt_count": 0,
                "confirmed_gross_earnings_base": 0,
                "confirmed_receipts": [],
            },
            "receipts": [
                {
                    "tx_hash": "0x" + tx * 64,
                    "job_id": "0x" + job * 64,
                    "worker": "0x" + "c" * 64,
                }
                for tx, job in (("d", "e"), ("f", "1"))
            ],
            "canonical_cutoff": {
                "block_height": 137148,
                "block_hash": "0x" + "7" * 64,
                "index": 1,
            },
        }
        self.reward_path = root / "reward.json"
        write_json(self.reward_path, self.reward)

        sources = [
            {
                "id": f"v3-{name}",
                "name": f"ARC v3 {name.upper()}",
                "region": name.upper(),
                "kind": "v3",
                "baseUrl": f"https://{host}",
                "enabled": True,
                "replicaGroup": "rollout",
            }
            for name, host in TRUTH.PRODUCTION_FLEET
        ]
        sources.extend(
            {
                "id": f"legacy-fork-{name}",
                "name": f"Preserved legacy fork · {name.upper()}",
                "region": name.upper(),
                "kind": "legacy-fork",
                "baseUrl": f"https://{host}/legacy/{name}",
                "enabled": True,
                "replicaGroup": "legacy-capture-" + "a" * 64,
                "description": "Explicit immutable historical fork; diagnostic only and never canonical.",
                "archive": {
                    "schema": "arc.legacy-archive.source.v1",
                    "readOnly": True,
                    "classification": "valid_noncanonical_fork",
                    "captureId": "a" * 64,
                    "node": name,
                    "rolloutManifestSha256": "b" * 64,
                    "archiveManifestSha256": "c" * 64,
                    "completeSha256": "d" * 64,
                    "bundleSha256": "e" * 64,
                    "inventorySha256": "f" * 64,
                    "bindingIndexSha256": "1" * 64,
                    "bindingSha256": "2" * 64,
                    "checkpointSha256": "3" * 64,
                    "checkpointManifestHash": "4" * 64,
                    "checkpointPayloadHash": "5" * 64,
                    "canonicalCheckpointHeight": 137145,
                    "sourceHeight": 141000,
                    "sourceBlockHash": "6" * 64,
                    "sourceStateRoot": "7" * 64,
                    "provenancePath": "/provenance",
                },
            }
            for name, host in TRUTH.PRODUCTION_FLEET
        )
        self.config = {
            "schema": TRUTH.NETWORK_SCHEMA,
            "state": "recovered",
            "network": {"name": "ARC Testnet", "chainId": TRUTH.CHAIN_ID},
            "checkpoint": {
                "height": 137145,
                "recoveryHeight": 137146,
                "legacyPublicMaxHeight": 141000,
                "blockHash": "2" * 64,
                "stateRoot": "3" * 64,
                "manifestHash": "4" * 64,
                "boundaryBlockHash": "5" * 64,
                "boundaryStateRoot": "6" * 64,
                "recoveryEpoch": 1,
                "validatorSetId": 1,
                "protocolVersion": "3.0.0",
                "recoveryDomain": "7" * 64,
                "legacySourceId": "v3-nyc",
                "v3SourceId": "v3-nyc",
            },
            "sources": sources,
            "services": {
                "maintenanceInterlock": {
                    "schema": "arc.frontend.maintenance-interlock.v1",
                    "path": "/maintenance/status",
                    "sourceMainCommit": self.source_sha,
                    "observedCutoffHeight": 141000,
                    "sourceSetSha256": "8" * 64,
                    "boundarySha256": "9" * 64,
                    "toolSha256": "a" * 64,
                    "requiredHealthyReplicas": 6,
                    "maxStalenessSeconds": 90,
                }
            },
            "notices": ["Recovered production network."],
        }
        self.config_path = root / "config.json"
        config_raw = write_json(self.config_path, self.config)
        self.deployed_commit = root / "deployed-commit.txt"
        self.deployed_commit.write_text(self.frontend_sha + "\n", encoding="ascii")
        self.deployed_sums = root / "deployed-SHA256SUMS"
        self.deployed_sums.write_text(
            f"{digest(self.deployed_commit.read_bytes())}  ./deployed-commit.txt\n"
            f"{digest(config_raw)}  ./shared/frontend/arc-network.json\n",
            encoding="ascii",
        )
        self._build_pages_evidence()

        self.installer_sha = digest(b"published installer bytes\n")
        self.assets = {
            name: {
                "id": 1000 + index,
                "sha256": self.installer_sha if name == "install.sh" else digest(name.encode()),
                "size": 32 + index,
            }
            for index, name in enumerate(sorted(PUBLISHED.EXPECTED_RELEASE_ASSETS))
        }
        self.release = {
            "id": 99,
            "tag_name": TRUTH.TAG,
            "target_commitish": self.source_sha,
            "draft": False,
            "prerelease": False,
            "immutable": True,
            "author": {"login": "github-actions[bot]"},
            "html_url": f"https://github.com/{TRUTH.REPOSITORY}/releases/tag/{TRUTH.TAG}",
            "published_at": "2026-09-06T01:02:03Z",
            "assets": [
                {
                    "id": row["id"],
                    "name": name,
                    "digest": "sha256:" + row["sha256"],
                    "state": "uploaded",
                    "size": row["size"],
                    "uploader": {"login": "github-actions[bot]"},
                    "browser_download_url": (
                        f"https://github.com/{TRUTH.REPOSITORY}/releases/download/"
                        f"{TRUTH.TAG}/{name}"
                    ),
                }
                for name, row in sorted(self.assets.items())
            ],
        }
        self.release_path = root / "release.json"
        write_json(self.release_path, self.release, canonical=False)
        self._build_published_evidence()

    def _build_pages_evidence(self) -> None:
        self.pages_workflow = self.root / "pages-workflow.json"
        write_json(self.pages_workflow, {"id": 11, "name": "Deploy ARC public console", "path": TRUTH.PAGES_WORKFLOW_PATH, "state": "active"}, canonical=False)
        self.pages_run = self.root / "pages-run.json"
        write_json(self.pages_run, {"id": 12, "run_attempt": 2, "workflow_id": 11, "head_repository": {"full_name": TRUTH.REPOSITORY}, "path": TRUTH.PAGES_WORKFLOW_PATH, "event": "push", "head_branch": "main", "head_sha": self.frontend_sha, "status": "completed", "conclusion": "success"}, canonical=False)
        self.pages_jobs_value = [{"id": 120 + index, "name": name, "run_id": 12, "run_attempt": 2, "head_sha": self.frontend_sha, "status": "completed", "conclusion": "success"} for index, name in enumerate(sorted(TRUTH.PAGES_JOB_NAMES))]
        self.pages_jobs = self.root / "pages-jobs.json"
        write_json(self.pages_jobs, self.pages_jobs_value, canonical=False)
        self.pages_api = self.root / "pages-api.json"
        write_json(self.pages_api, {"build_type": "workflow", "html_url": TRUTH.PUBLIC_CONSOLE}, canonical=False)
        self.pages_deployments = self.root / "pages-deployments.json"
        write_json(self.pages_deployments, [{"id": 13, "sha": self.frontend_sha, "ref": "main", "environment": "github-pages", "task": "deploy"}], canonical=False)
        self.pages_statuses_value = [{"id": 130, "state": "success", "environment": "github-pages", "environment_url": TRUTH.PUBLIC_CONSOLE}, {"id": 129, "state": "in_progress", "environment": "github-pages", "environment_url": TRUTH.PUBLIC_CONSOLE}]
        self.pages_statuses = self.root / "pages-statuses.json"
        write_json(self.pages_statuses, self.pages_statuses_value, canonical=False)

    def _build_published_evidence(self) -> None:
        self.published_workflow = self.root / "published-workflow.json"
        write_json(self.published_workflow, {"id": 13, "name": "Published artifact acceptance", "path": TRUTH.PUBLISHED_WORKFLOW_PATH, "state": "active"}, canonical=False)
        self.published_run = self.root / "published-run.json"
        write_json(self.published_run, {"id": self.acceptance_run_id, "run_attempt": self.acceptance_run_attempt, "workflow_id": 13, "head_repository": {"full_name": TRUTH.REPOSITORY}, "path": TRUTH.PUBLISHED_WORKFLOW_PATH, "event": "workflow_dispatch", "head_branch": TRUTH.TAG, "head_sha": self.source_sha, "status": "completed", "conclusion": "success"}, canonical=False)
        self.published_jobs_value = [{"id": 140 + index, "name": name, "run_id": self.acceptance_run_id, "run_attempt": self.acceptance_run_attempt, "head_sha": self.source_sha, "status": "completed", "conclusion": "success"} for index, name in enumerate(sorted(TRUTH.PUBLISHED_JOB_NAMES))]
        self.published_jobs = self.root / "published-jobs.json"
        write_json(self.published_jobs, self.published_jobs_value, canonical=False)
        self.artifact_root = self.root / "published-artifact"
        self.artifact_root.mkdir(mode=0o700)
        evidence_root = self.artifact_root / "evidence"
        payloads = {name: f"evidence for {name}\n".encode() for name in TRUTH.PUBLISHED_EVIDENCE_FILES}
        for name, raw in payloads.items():
            path = evidence_root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(raw)
        evidence_hashes = {name: {"sha256": digest(raw), "size": len(raw)} for name, raw in sorted(payloads.items())}
        release_jobs_sha = evidence_hashes["release/release-attempt-jobs.json"]["sha256"]
        release_published_sha = evidence_hashes["release/release-published.json"]["sha256"]
        published_metadata_sha = evidence_hashes["release/published-evidence-artifact.json"]["sha256"]
        binding = {
            "assets": self.assets,
            "commit": self.source_sha,
            "legacy_source": PUBLISHED.LEGACY_SOURCE,
            "published_evidence": {"artifact_digest": "sha256:" + evidence_hashes["release/published-evidence.zip"]["sha256"], "artifact_id": 88, "artifact_metadata_sha256": published_metadata_sha, "artifact_name": f"arc-release-published-evidence-{self.source_sha}-{self.release_run_id}-{self.release_run_attempt}-99-{release_published_sha}", "artifact_size": 111, "release_published_sha256": release_published_sha},
            "release": {"id": 99, "immutable": True},
            "release_workflow": {"event": "workflow_dispatch", "head_branch": "main", "head_sha": self.source_sha, "id": 7, "jobs_sha256": release_jobs_sha, "path": ".github/workflows/release.yml", "run_attempt": self.release_run_attempt, "run_id": self.release_run_id},
            "repository": TRUTH.REPOSITORY,
            "schema": "arc.published-release-binding.v1",
            "tag": TRUTH.TAG,
        }
        binding_path = self.artifact_root / "release-binding.json"
        binding_sha = digest(write_json(binding_path, binding))
        component_artifacts = {"schema": "arc.published-artifact-component-binding.v1", "repository": TRUTH.REPOSITORY, "acceptance_run_id": self.acceptance_run_id, "acceptance_run_attempt": self.acceptance_run_attempt, "artifacts": {}}
        platforms = sorted(PUBLISHED.EXPECTED_COMPONENTS)
        for index, platform in enumerate(platforms):
            component_artifacts["artifacts"][platform] = {"name": f"arc-published-acceptance-{platform}-{self.acceptance_run_id}-attempt-{self.acceptance_run_attempt}", "id": 200 + index, "digest": "sha256:" + digest(platform.encode()), "size": 100 + index, "expired": False, "workflow_run_id": self.acceptance_run_id, "head_sha": self.source_sha}
        component_artifacts_path = self.artifact_root / "component-artifacts.json"
        component_artifacts_raw = write_json(component_artifacts_path, component_artifacts)
        component_receipts = self.root / "component-receipts"
        component_receipts.mkdir()
        for platform in platforms:
            checks = dict(PUBLISHED.REQUIRED_CHECKS[platform])
            if platform == "windows-x86_64":
                checks["embedded_app_product_version"] = "0.8.0"
            component = {"acceptance_run_attempt": self.acceptance_run_attempt, "acceptance_run_id": self.acceptance_run_id, "assets": {name: self.assets[name] for name in PUBLISHED.EXPECTED_COMPONENTS[platform]}, "binding_sha256": binding_sha, "checks": checks, "commit": self.source_sha, "platform": platform, "release_id": 99, "release_run_attempt": self.release_run_attempt, "release_run_id": self.release_run_id, "repository": TRUTH.REPOSITORY, "schema": "arc.published-artifact-acceptance-component.v1", "tag": TRUTH.TAG}
            component_path = component_receipts / f"{platform}.json"
            write_json(component_path, component)
            os.link(component_path, self.artifact_root / f"{platform}.json")
        evidence_manifest = {"acceptance_run_attempt": self.acceptance_run_attempt, "acceptance_run_id": self.acceptance_run_id, "binding_sha256": binding_sha, "component_artifact_binding_sha256": digest(component_artifacts_raw), "files": evidence_hashes, "repository": TRUTH.REPOSITORY, "schema": "arc.published-artifact-evidence-manifest.v1"}
        evidence_manifest_path = self.artifact_root / TRUTH.PUBLISHED_EVIDENCE_MANIFEST
        write_json(evidence_manifest_path, evidence_manifest)
        PUBLISHED.command_aggregate(argparse.Namespace(binding=binding_path, component_artifacts=component_artifacts_path, components=component_receipts, evidence_manifest=evidence_manifest_path, evidence_root=evidence_root, acceptance_run_id=self.acceptance_run_id, acceptance_run_attempt=self.acceptance_run_attempt, output=self.artifact_root / TRUTH.PUBLISHED_ACCEPTANCE_RECEIPT))
        self.rebuild_published_zip()

    def rebuild_published_zip(self) -> None:
        sums_path = self.artifact_root / TRUTH.PUBLISHED_ACCEPTANCE_SUMS
        sums_path.write_text("".join(f"{digest(path.read_bytes())}  ./{path.relative_to(self.artifact_root).as_posix()}\n" for path in sorted(self.artifact_root.rglob("*")) if path.is_file() and path != sums_path), encoding="ascii")
        self.published_zip = self.root / "published-acceptance.zip"
        with zipfile.ZipFile(self.published_zip, "w", zipfile.ZIP_DEFLATED) as archive:
            for path in sorted(self.artifact_root.rglob("*")):
                if path.is_file():
                    archive.write(path, path.relative_to(self.artifact_root).as_posix())
        self.published_artifact_metadata = self.root / "published-artifact-metadata.json"
        write_json(self.published_artifact_metadata, {"id": 15, "name": f"arc-published-artifact-acceptance-{TRUTH.TAG}-{self.source_sha}-{self.acceptance_run_id}-attempt-{self.acceptance_run_attempt}", "size_in_bytes": self.published_zip.stat().st_size, "digest": "sha256:" + digest(self.published_zip.read_bytes()), "expired": False, "workflow_run": {"id": self.acceptance_run_id, "head_sha": self.source_sha}}, canonical=False)

    def args(self, output: Path, **overrides: object) -> argparse.Namespace:
        values: dict[str, object] = {"readme": self.readme, "release_api": self.release_path, "pages_workflow": self.pages_workflow, "pages_run": self.pages_run, "pages_jobs": self.pages_jobs, "pages_api": self.pages_api, "pages_deployments": self.pages_deployments, "pages_statuses": self.pages_statuses, "frontend_config": self.config_path, "deployed_commit": self.deployed_commit, "deployed_sha256sums": self.deployed_sums, "published_workflow": self.published_workflow, "published_run": self.published_run, "published_jobs": self.published_jobs, "published_artifact_metadata": self.published_artifact_metadata, "published_artifact_zip": self.published_zip, "reward_evidence": self.reward_path, "rollout_manifest": self.manifest_path, "output_dir": output}
        values.update(overrides)
        return argparse.Namespace(**values)


class PublicTruthTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.fixture = Fixture(self.root)
        self.recovery_calls: list[tuple[Path, Path]] = []

        def verified(manifest, reward, manifest_raw, reward_raw, temporary_root):
            self.recovery_calls.append((manifest, reward))
            return {"manifestSha256": digest(manifest_raw), "rewardEvidenceSha256": digest(reward_raw), "stdoutSha256": "9" * 64, "verifierPath": TRUTH.RECOVERY_VERIFIER_RELATIVE.as_posix(), "verifierSha256": "8" * 64}

        self.recovery_patch = mock.patch.object(TRUTH, "run_recovery_verify", side_effect=verified)
        self.recovery_patch.start()

    def tearDown(self) -> None:
        self.recovery_patch.stop()
        self.temporary.cleanup()

    def test_builds_v2_receipt_and_claims_from_raw_evidence(self) -> None:
        output = self.root / "output"
        readme_path, status_path = TRUTH.build(self.fixture.args(output))
        acceptance_path = output / "POST-RELEASE-ACCEPTANCE.json"
        self.assertEqual({path.name for path in output.iterdir()}, {"README.md", "production-status.json", "POST-RELEASE-ACCEPTANCE.json"})
        for path in (readme_path, status_path, acceptance_path):
            self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o400)
        readme = readme_path.read_text(encoding="utf-8")
        self.assertIn("published Linux x86_64 component proved", readme)
        self.assertIn(f"ARC_INSTALL_SHA256={self.fixture.installer_sha}", readme)
        acceptance_raw = acceptance_path.read_bytes()
        acceptance = json.loads(acceptance_raw)
        self.assertEqual(acceptance["schema"], "arc.post-release-acceptance.v2")
        self.assertNotIn("pages_jobs_succeeded", acceptance)
        self.assertEqual(acceptance["publishedAcceptance"]["componentReceiptSha256"]["linux-x86_64"], digest((self.fixture.artifact_root / "linux-x86_64.json").read_bytes()))
        status = json.loads(status_path.read_bytes())
        self.assertEqual(status["acceptance"]["receiptSha256"], digest(acceptance_raw))
        self.assertEqual(status["acceptance"]["receipt"], acceptance)
        self.assertEqual(
            digest(TRUTH.canonical_json(status["acceptance"]["receipt"])),
            status["acceptance"]["receiptSha256"],
        )
        self.assertEqual(
            status["acceptance"]["receipt"]["publishedAcceptance"]["artifactId"],
            15,
        )
        self.assertEqual(
            status["acceptance"]["receipt"]["publishedAcceptance"]["runId"],
            self.fixture.acceptance_run_id,
        )
        self.assertEqual(status["pages"]["acceptedConfigCommit"], self.fixture.frontend_sha)
        self.assertEqual(status["rewards"]["demonstratedGrossBase"], 5_000_000_000)
        self.assertEqual(self.recovery_calls, [(self.fixture.manifest_path, self.fixture.reward_path)])

    def test_rejects_wrong_attempt_job_even_if_run_says_success(self) -> None:
        jobs = copy.deepcopy(self.fixture.pages_jobs_value)
        jobs[0]["run_attempt"] = 1
        path = self.root / "bad-pages-jobs.json"
        write_json(path, jobs, canonical=False)
        with self.assertRaisesRegex(TRUTH.TruthError, "exact successful attempt"):
            TRUTH.build(self.fixture.args(self.root / "bad-jobs-output", pages_jobs=path))
        self.assertFalse(self.recovery_calls)

    def test_rejects_stale_pages_success_and_tampered_cdn_commit(self) -> None:
        statuses = list(reversed(self.fixture.pages_statuses_value))
        statuses_path = self.root / "stale-statuses.json"
        write_json(statuses_path, statuses, canonical=False)
        with self.assertRaisesRegex(TRUTH.TruthError, "latest exact Pages"):
            TRUTH.build(self.fixture.args(self.root / "stale-output", pages_statuses=statuses_path))
        commit_path = self.root / "attacker-commit.txt"
        commit_path.write_text("c" * 40 + "\n", encoding="ascii")
        with self.assertRaisesRegex(TRUTH.TruthError, "deployed-commit"):
            TRUTH.build(self.fixture.args(self.root / "cdn-output", deployed_commit=commit_path))

    def test_rejects_artifact_metadata_digest_or_rehashed_component_forgery(self) -> None:
        metadata = json.loads(self.fixture.published_artifact_metadata.read_text())
        metadata["digest"] = "sha256:" + "0" * 64
        metadata_path = self.root / "wrong-artifact-metadata.json"
        write_json(metadata_path, metadata, canonical=False)
        with self.assertRaisesRegex(TRUTH.TruthError, "ZIP bytes differ"):
            TRUTH.build(self.fixture.args(self.root / "wrong-artifact-output", published_artifact_metadata=metadata_path))
        linux_path = self.fixture.artifact_root / "linux-x86_64.json"
        linux = json.loads(linux_path.read_text())
        linux["assets"]["install.sh"]["sha256"] = "0" * 64
        write_json(linux_path, linux)
        self.fixture.rebuild_published_zip()
        with self.assertRaisesRegex(TRUTH.TruthError, "component linux-x86_64 hash"):
            TRUTH.build(self.fixture.args(self.root / "forged-component-output"))

    def test_rejects_zip_traversal_or_uncovered_member(self) -> None:
        unsafe_zip = self.root / "unsafe.zip"
        with zipfile.ZipFile(unsafe_zip, "w") as archive:
            for path in self.fixture.artifact_root.rglob("*"):
                if path.is_file():
                    archive.write(path, path.relative_to(self.fixture.artifact_root).as_posix())
            archive.writestr("../escape", b"attack")
        metadata = json.loads(self.fixture.published_artifact_metadata.read_text())
        metadata["size_in_bytes"] = unsafe_zip.stat().st_size
        metadata["digest"] = "sha256:" + digest(unsafe_zip.read_bytes())
        metadata_path = self.root / "unsafe-metadata.json"
        write_json(metadata_path, metadata, canonical=False)
        with self.assertRaisesRegex(TRUTH.TruthError, "exact canonical file set"):
            TRUTH.build(self.fixture.args(self.root / "unsafe-output", published_artifact_zip=unsafe_zip, published_artifact_metadata=metadata_path))

    def test_live_recovery_verifier_failure_creates_no_output(self) -> None:
        self.recovery_patch.stop()
        output = self.root / "verify-failed-output"
        with mock.patch.object(TRUTH, "run_recovery_verify", side_effect=TRUTH.TruthError("live convergence failed")):
            with self.assertRaisesRegex(TRUTH.TruthError, "live convergence failed"):
                TRUTH.build(self.fixture.args(output))
        self.recovery_patch.start()
        self.assertFalse(output.exists())

    def test_rejects_mutable_release_duplicate_reward_and_existing_output(self) -> None:
        release = copy.deepcopy(self.fixture.release)
        release["immutable"] = False
        release_path = self.root / "mutable-release.json"
        write_json(release_path, release, canonical=False)
        with self.assertRaisesRegex(TRUTH.TruthError, "immutable"):
            TRUTH.build(self.fixture.args(self.root / "mutable-output", release_api=release_path))
        reward = copy.deepcopy(self.fixture.reward)
        reward["receipts"][1]["tx_hash"] = reward["receipts"][0]["tx_hash"]
        reward_path = self.root / "duplicate-reward.json"
        write_json(reward_path, reward)
        with self.assertRaisesRegex(TRUTH.TruthError, "distinct"):
            TRUTH.build(self.fixture.args(self.root / "reward-output", reward_evidence=reward_path))
        existing = self.root / "existing"
        existing.mkdir()
        with self.assertRaisesRegex(TRUTH.TruthError, "cannot create"):
            TRUTH.build(self.fixture.args(existing))

    def test_rejects_noncanonical_or_symlink_inputs(self) -> None:
        noncanonical = self.root / "noncanonical-config.json"
        write_json(noncanonical, self.fixture.config, canonical=False)
        with self.assertRaisesRegex(TRUTH.TruthError, "not canonical"):
            TRUTH.build(self.fixture.args(self.root / "noncanonical-output", frontend_config=noncanonical))
        linked = self.root / "linked-readme.md"
        linked.symlink_to(self.fixture.readme)
        with self.assertRaisesRegex(TRUTH.TruthError, "cannot read README"):
            TRUTH.build(self.fixture.args(self.root / "linked-output", readme=linked))


if __name__ == "__main__":
    unittest.main()
