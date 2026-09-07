from __future__ import annotations

import argparse
import copy
import hashlib
import importlib.util
import json
import os
import shutil
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
        self.node_path = root / "node-v24.20.0"
        self.node_path.write_bytes(b"fixture node bytes")
        self.node_sha256 = digest(self.node_path.read_bytes())
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
            "chain": {
                "source_height": 137_145,
                "transition_height": 137_146,
                "legacy_public_max_height": 141_128,
                "source_block_hash": "2" * 64,
                "source_state_root": "3" * 64,
                "canonical_source": {
                    "node": "nyc",
                },
            },
        }
        self.manifest_path = root / "manifest.json"
        manifest_raw = write_json(self.manifest_path, self.manifest)
        self.manifest_sha256 = digest(manifest_raw)
        self.manifest_path.chmod(0o400)
        self.manifest_sidecar = root / "manifest.json.sha256"
        self.manifest_sidecar.write_text(
            f"{digest(manifest_raw)}  manifest.json\n", encoding="ascii"
        )
        self.manifest_sidecar.chmod(0o400)
        self.reward = {
            "schema": TRUTH.REWARD_SCHEMA,
            "rollout_sha256": self.manifest_sha256,
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
                "legacyPublicMaxHeight": 141128,
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
        self._build_desktop_live_receipt(config_raw)

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

    def _build_desktop_live_receipt(self, config_raw: bytes) -> None:
        tx_hash = "0x" + "d" * 64
        job_id = "0x" + "e" * 64
        worker = "0x" + "c" * 64
        block_hash = "0x" + "7" * 64
        direct = {
            "assignment_epoch": "0x" + "2" * 64,
            "block_hash": block_hash,
            "block_height": 137148,
            "confirmed": True,
            "evidence_source": "successful mined CommunityInferenceReward receipt",
            "included": True,
            "index": 1,
            "input_hash": "0x" + "3" * 64,
            "job_id": job_id,
            "model_id": "0x" + "4" * 64,
            "output_hash": "0x" + "5" * 64,
            "receipt_url": f"/community/reward_receipt/{tx_hash}",
            "recovery_epoch": 1,
            "reward_arc": 2.5,
            "reward_base": 2_500_000_000,
            "status": "mined_success",
            "submitted": True,
            "success": True,
            "transaction_domain": "0x" + "6" * 64,
            "tx_hash": tx_hash,
            "tx_type": "0x25",
            "validator_approvals": 5,
            "validator_set_commitment": "0x" + "8" * 64,
            "validator_set_id": 1,
            "worker": worker,
        }
        earnings_receipt = {
            key: value
            for key, value in direct.items()
            if key in TRUTH.DESKTOP_EARNINGS_RECEIPT_KEYS
        }
        suite_hashes = {
            relative.as_posix(): TRUTH.repository_tool(
                relative, f"fixture desktop suite {relative.as_posix()}"
            )[1]
            for relative in TRUTH.DESKTOP_LIVE_SUITE_RELATIVES
        }
        package_lock, package_lock_sha = TRUTH.repository_tool(
            TRUTH.DESKTOP_PACKAGE_LOCK_RELATIVE, "fixture desktop package lock"
        )
        lock = json.loads(package_lock.read_text(encoding="utf-8"))
        native_challenge = "6" * 64
        native_assets = {
            key: {
                "id": self.assets[name]["id"],
                "name": name,
                "sha256": self.assets[name]["sha256"],
                "size": self.assets[name]["size"],
            }
            for key, name in {
                "appArchive": "arc-desktop-macos-arm64.app.tar.gz",
                "appArchiveSignature": "arc-desktop-macos-arm64.app.tar.gz.sig",
                "dmg": "arc-desktop-macos-arm64.dmg",
            }.items()
        }
        native_bundle = {
            "appBundleTreeSha256": "4" * 64,
            "executableRelativePath": "Contents/MacOS/arc-desktop",
            "executableSha256": "5" * 64,
            "executableSize": 1234,
        }
        native_input = {
            "assets": native_assets,
            "budgets": {
                "dispatchMaxWaitMs": 3_960_000,
                "earningsMaxPolls": 11,
                "earningsMaxWaitMs": 30_000,
                "finalReadMaxPolls": 11,
                "finalReadMaxWaitMs": 30_000,
                "preflightMaxWaitMs": 30_000,
                "receiptMaxPolls": 61,
                "receiptMaxWaitMs": 180_000,
                "totalMaxWaitMs": 4_300_000,
            },
            "challenge": native_challenge,
            "expectedActiveValidators": 6,
            "expectedBundle": native_bundle,
            "expectedCoordinator": "https://140.82.16.112",
            "expectedModelId": "0x" + "4" * 64,
            "expectedRegisteredValidators": 6,
            "expiresAtUnix": 2_000_007_200,
            "frontendCommit": self.source_sha,
            "frontendConfigSha256": digest(config_raw),
            "issuedAtUnix": 2_000_000_000,
            "maximumBlockAgeSeconds": 300,
            "minimumPeers": 5,
            "pagesOrigin": "https://ferrumvir.github.io/arc-chain",
            "recoveryEpoch": 1,
            "releaseId": 99,
            "releaseRunAttempt": self.release_run_attempt,
            "releaseRunId": self.release_run_id,
            "releaseVersion": "0.8.0",
            "repository": TRUTH.REPOSITORY,
            "rolloutManifestSha256": self.manifest_sha256,
            "schema": "arc.packaged-desktop-native-input.v1",
            "sourceCommit": self.source_sha,
            "transactionDomain": "0x" + "6" * 64,
            "validatorApprovalsRequired": 5,
            "validatorSetCommitment": "0x" + "8" * 64,
            "validatorSetId": 1,
        }
        native_input_raw = TRUTH.canonical_json(native_input)
        native_input_sha = digest(native_input_raw)
        native_prompt = (
            f"ARC packaged v0.8.0 production acceptance challenge {native_challenge} "
            f"input {native_input_sha}"
        )
        native_output = "accepted"
        native_terminal = {
            "txHash": tx_hash,
            "jobId": job_id,
            "worker": worker,
            "receiptUrl": f"/community/reward_receipt/{tx_hash}",
            "txType": "0x25",
            "submitted": True,
            "included": True,
            "confirmed": True,
            "success": True,
            "status": "mined_success",
            "modelId": "0x" + "4" * 64,
            "inputHash": "0x" + TRUTH.blake3_short(native_prompt.encode()),
            "outputHash": "0x" + TRUTH.blake3_short(native_output.encode()),
            "rewardArc": 2.5,
            "rewardBase": 2_500_000_000,
            "validatorApprovals": 5,
        }
        native_receipt = {
            "assets": native_assets,
            "bundle": native_bundle,
            "challenge": native_challenge,
            "dispatch": {
                "count": 1,
                "promptSha256": digest(native_prompt.encode()),
                "result": {
                    "input": native_prompt,
                    "modelHash": native_terminal["modelId"],
                    "output": native_output,
                    "outputHash": native_terminal["outputHash"],
                    "settlement": dict(native_terminal),
                },
            },
            "dispatchAttemptSha256": "7" * 64,
            "expectedCoordinator": "https://140.82.16.112",
            "expectedModelId": native_terminal["modelId"],
            "frontendConfigSha256": digest(config_raw),
            "immutableSessionOrigin": True,
            "inputSha256": native_input_sha,
            "negativeRouteChecks": {
                "job": True, "origin": True, "receiptUrl": True,
                "transaction": True, "worker": True,
            },
            "network": {"sourceHost": "https://140.82.16.112"},
            "receiptPoll": {"receipt": dict(native_terminal)},
            "repository": TRUTH.REPOSITORY,
            "rolloutManifestSha256": self.manifest_sha256,
            "runtime": {
                "appDataRelativePath": "Library/Application Support/network.arc.desktop",
                "appVersion": "0.8.0",
                "architecture": "aarch64",
                "buildSourceCommit": self.source_sha,
                "environmentNames": ["HOME", "LANG", "LC_ALL", "PATH", "TMPDIR"],
                "environmentSha256": "9" * 64,
                "ipcHandlersRegistered": False,
                "isolatedHomeBasename": "isolated-home",
                "operatingSystem": "macos",
                "pluginsLoaded": False,
                "tauriBuilderStarted": False,
                "webviewsCreated": 0,
            },
            "schema": "arc.packaged-desktop-native-acceptance.v1",
            "sourceCommit": self.source_sha,
            "transaction": {"sourceHost": "https://140.82.16.112"},
            "worker": worker,
        }
        native_receipt_raw = TRUTH.canonical_json(native_receipt)
        native_attempt = {
            "armedAt": "2026-09-06T12:00:00Z",
            "challenge": native_challenge,
            "dispatchLimit": 1,
            "executableSha256": native_bundle["executableSha256"],
            "inputSha256": native_input_sha,
            "schema": "arc.packaged-desktop-native-dispatch-attempt.v1",
            "sourceCommit": self.source_sha,
            "sourceHost": native_input["expectedCoordinator"],
        }
        native_attempt_raw = TRUTH.canonical_json(native_attempt)
        native_receipt["dispatchAttemptSha256"] = digest(native_attempt_raw)
        native_receipt_raw = TRUTH.canonical_json(native_receipt)

        _controller, controller_sha = TRUTH.repository_tool(
            TRUTH.MACOS_PACKAGE_CONTROLLER_RELATIVE,
            "fixture macOS package controller",
        )
        controller_source = {
            "commit": self.source_sha,
            "path": TRUTH.MACOS_PACKAGE_CONTROLLER_RELATIVE.as_posix(),
            "sha256": controller_sha,
            "treeClean": True,
        }
        macos_release = {
            "id": native_input["releaseId"],
            "runAttempt": native_input["releaseRunAttempt"],
            "runId": native_input["releaseRunId"],
            "tag": TRUTH.TAG,
        }
        binding_sha = "a" * 64
        tool = lambda path, marker: {
            "path": path,
            "resolvedPath": path,
            "sha256": marker * 64,
            "size": 1,
        }
        code_signature = {
            "appleDeveloperIdSigned": False,
            "authorities": [],
            "designatedRequirement": 'designated => identifier "network.arc.desktop"',
            "designatedRequirementKind": "identifier",
            "displayStderrSha256": "1" * 64,
            "displayStdoutSha256": "2" * 64,
            "gatekeeperAssessed": False,
            "hardenedRuntime": False,
            "identifier": "network.arc.desktop",
            "infoPlistSha256": "3" * 64,
            "kind": "adhoc",
            "notarizationAssessed": False,
            "requirementsStderrSha256": "4" * 64,
            "requirementsStdoutSha256": "5" * 64,
            "teamIdentifier": None,
            "verifier": tool("/usr/bin/codesign", "6"),
            "verifyDeepStrict": True,
            "verifyStderrSha256": "7" * 64,
            "verifyStdoutSha256": "8" * 64,
        }
        def mount_evidence(basename: str) -> dict[str, object]:
            return {
                "attachPlistSha256": "1" * 64,
                "device": "/dev/disk4s1",
                "diskutilInfoPlistSha256": "2" * 64,
                "filesystem": "apfs",
                "hdiutilInfoPlistSha256": "3" * 64,
                "imagePathMatched": True,
                "imagePathSha256": "4" * 64,
                "mountFlags": {"lineSha256": "5" * 64, "options": ["nobrowse", "noowners", "read-only"]},
                "mountPointBasename": basename,
                "nobrowse": True,
                "noowners": True,
                "readOnlyMedia": True,
                "readOnlyVolume": True,
                "statvfsReadOnly": True,
                "tools": {
                    "codesign": tool("/usr/bin/codesign", "6"),
                    "diskutil": tool("/usr/sbin/diskutil", "7"),
                    "hdiutil": tool("/usr/bin/hdiutil", "8"),
                    "mount": tool("/sbin/mount", "9"),
                },
                "verifyStderrSha256": "a" * 64,
                "verifyStdoutSha256": "b" * 64,
                "writeable": False,
            }
        detach = {
            "detachStderrSha256": "c" * 64,
            "detachStdoutSha256": "d" * 64,
            "detached": True,
            "mountPointEmpty": True,
            "postDetachInfoPlistSha256": "e" * 64,
        }
        tree = {"entryCount": 3, "sha256": native_bundle["appBundleTreeSha256"], "totalRegularBytes": 1234}
        guest = {
            "apt": {
                "installStderrSha256": "1" * 64,
                "installStdoutSha256": "2" * 64,
                "snapshot": "20260321T235959Z",
                "sourceSha256": "3" * 64,
                "updateStderrSha256": "4" * 64,
                "updateStdoutSha256": "5" * 64,
            },
            "archiveSha256": native_assets["appArchive"]["sha256"],
            "completedAt": "2026-09-06T12:00:00Z",
            "helperSha256": controller_sha,
            "minisign": {
                "binary": tool("/usr/bin/minisign", "6"),
                "package": {"architecture": "amd64", "version": "0.11-1"},
            },
            "publicKeySha256": "7" * 64,
            "schema": "arc.macos-updater-signature-guest.v1",
            "signatureSha256": native_assets["appArchiveSignature"]["sha256"],
            "verification": {"stderrSha256": "8" * 64, "stdoutSha256": "9" * 64, "verified": True},
        }
        signature_receipt = {
            "assets": {"appArchive": native_assets["appArchive"], "appArchiveSignature": native_assets["appArchiveSignature"]},
            "bindingSha256": binding_sha,
            "completedAt": "2026-09-06T12:00:01Z",
            "disposableVm": {
                "configSha256": "b" * 64,
                "deletedAfterEvidenceRead": True,
                "imageDigest": "sha256:5c3ddb00f60bc455dac0862fabe9d8bacec46c33ac1751143c5c3683404b110d",
                "imageUrl": "https://cloud-images.ubuntu.com/releases/noble/release-20260321/ubuntu-24.04-server-cloudimg-amd64.img",
                "mounts": [],
                "name": f"arc-macos-signature-v080-{self.source_sha[:8]}-abc123",
                "preexistingInstancesUnchanged": True,
                "recoveryEnclaveAccessed": False,
            },
            "guest": guest,
            "guestReceiptSha256": digest(TRUTH.canonical_json(guest)),
            "limactl": tool("/opt/homebrew/bin/limactl", "c"),
            "release": macos_release,
            "repository": TRUTH.REPOSITORY,
            "schema": TRUTH.MACOS_UPDATER_SIGNATURE_SCHEMA,
            "source": controller_source,
            "sourceCommit": self.source_sha,
            "updaterPublicKeySha256": guest["publicKeySha256"],
            "verified": True,
        }
        signature_raw = TRUTH.canonical_json(signature_receipt)
        extraction = {
            "appBundleRelativePath": "ARC Node.app",
            "appBundleTreeSha256": native_bundle["appBundleTreeSha256"],
            "archiveSha256AfterExtraction": native_assets["appArchive"]["sha256"],
            "implementation": "arc-openat-create-only-tar-extractor-v1",
            "limits": {"maxDepth": 33, "maxEntries": 20000, "maxMemberBytes": 536870912, "maxPathBytes": 4096, "maxTotalRegularBytes": 2147483648},
            "manifest": {"directoryCount": 2, "memberCount": 4, "regularFileCount": 2, "symlinkCount": 0, "totalRegularBytes": 1234},
            "safety": {
                "absolutePathsRejected": True,
                "caseAndNormalizationCollisionsRejected": True,
                "descriptorRelativeCreateOnly": True,
                "hardlinksAndSpecialEntriesRejected": True,
                "parentTraversalRejected": True,
                "setIdAndStickyModesRejected": True,
                "symlinksResolvedInsideBundle": True,
            },
        }
        inspection = {
            "assets": native_assets,
            "bindingSha256": binding_sha,
            "bundle": native_bundle,
            "codeSignature": code_signature,
            "completedAt": "2026-09-06T12:00:02Z",
            "dmg": {
                "appBundleTree": tree,
                "attach": mount_evidence("inspect-dmg-mount"),
                "detach": detach,
                "rootInventory": [{"kind": "directory", "mode": 493, "name": "ARC Node.app", "target": None}],
                "sha256AfterDetach": native_assets["dmg"]["sha256"],
            },
            "extraction": extraction,
            "release": macos_release,
            "repository": TRUTH.REPOSITORY,
            "schema": TRUTH.MACOS_PACKAGE_INSPECTION_SCHEMA,
            "source": controller_source,
            "sourceCommit": self.source_sha,
            "updaterSignature": {"receiptSha256": digest(signature_raw), "schema": TRUTH.MACOS_UPDATER_SIGNATURE_SCHEMA, "updaterPublicKeySha256": signature_receipt["updaterPublicKeySha256"], "verified": True},
        }
        inspection_raw = TRUTH.canonical_json(inspection)
        controller_attempt = {
            "armedAt": "2026-09-06T12:00:03Z",
            "challenge": native_challenge,
            "executableSha256": native_bundle["executableSha256"],
            "inputSha256": native_input_sha,
            "inspectionSha256": digest(inspection_raw),
            "outerTimeoutSeconds": 4360,
            "retryPermitted": False,
            "schema": "arc.macos-native-controller-attempt.v1",
            "sourceCommit": self.source_sha,
            "state": "armed-no-retry",
        }
        controller_attempt_raw = TRUTH.canonical_json(controller_attempt)
        provenance = {
            "assets": native_assets,
            "bindingSha256": binding_sha,
            "bundle": native_bundle,
            "codeSignature": {"after": code_signature, "before": code_signature, "semanticIdentityUnchanged": True},
            "completedAt": "2026-09-06T12:00:05Z",
            "controllerAttemptSha256": digest(controller_attempt_raw),
            "dmgExecution": {"attach": mount_evidence("native-dmg-mount"), "bundleAfter": native_bundle, "bundleBefore": native_bundle, "detach": detach, "treeAfter": tree, "treeBefore": tree},
            "extractedArchiveBundleAfter": native_bundle,
            "inspectionSha256": digest(inspection_raw),
            "nativeDispatchAttemptSha256": digest(native_attempt_raw),
            "nativeExecution": {"completedAt": "2026-09-06T12:00:05Z", "elapsedMs": 1000, "innerMaxSeconds": 4300, "outerTimeoutSeconds": 4360, "returnCode": 0, "startedAt": "2026-09-06T12:00:04Z", "stderrSha256": "d" * 64, "stderrSize": 0, "stdoutSha256": "e" * 64, "stdoutSize": 1, "timedOut": False},
            "nativeInputSha256": native_input_sha,
            "nativeReceiptSha256": digest(native_receipt_raw),
            "release": macos_release,
            "repository": TRUTH.REPOSITORY,
            "schema": TRUTH.MACOS_PACKAGE_PROVENANCE_SCHEMA,
            "source": controller_source,
            "sourceCommit": self.source_sha,
            "truthScope": dict(TRUTH.MACOS_PACKAGE_TRUTH_SCOPE),
            "updaterSignatureReceiptSha256": digest(signature_raw),
        }
        provenance_raw = TRUTH.canonical_json(provenance)
        verification = {
            "assets": native_assets,
            "bindingSha256": binding_sha,
            "bundle": native_bundle,
            "completedAt": "2026-09-06T12:00:06Z",
            "controllerAttemptSha256": digest(controller_attempt_raw),
            "inspectionSha256": digest(inspection_raw),
            "nativeDispatchAttemptSha256": digest(native_attempt_raw),
            "nativeInputSha256": native_input_sha,
            "nativeReceiptSha256": digest(native_receipt_raw),
            "provenanceSha256": digest(provenance_raw),
            "release": macos_release,
            "repository": TRUTH.REPOSITORY,
            "schema": TRUTH.MACOS_PACKAGE_PROVENANCE_VERIFICATION_SCHEMA,
            "source": controller_source,
            "sourceCommit": self.source_sha,
            "updaterSignatureReceiptSha256": digest(signature_raw),
            "verified": True,
        }
        verification_raw = TRUTH.canonical_json(verification)
        self.macos_package_evidence_dir = self.root / "macos-package-evidence"
        self.macos_package_evidence_dir.mkdir(mode=0o700)
        evidence_values = {
            "MACOS-NATIVE-CONTROLLER-ATTEMPT.json": controller_attempt,
            "MACOS-PACKAGE-INSPECTION.json": inspection,
            "MACOS-PACKAGE-PROVENANCE-VERIFICATION.json": verification,
            "MACOS-PACKAGE-PROVENANCE.json": provenance,
            "MACOS-UPDATER-SIGNATURE.json": signature_receipt,
            "DESKTOP-LIVE-INPUT.json": native_input,
            "PACKAGED-NATIVE-ACCEPTANCE.json": native_receipt,
            "PACKAGED-NATIVE-DISPATCH-ATTEMPT.json": native_attempt,
        }
        for name, value in evidence_values.items():
            path = self.macos_package_evidence_dir / name
            write_json(path, value)
            path.chmod(0o400)
        native_wrapper = {
            "appArchiveSha256": native_assets["appArchive"]["sha256"],
            "appArchiveSignatureSha256": native_assets["appArchiveSignature"]["sha256"],
            "appBundleTreeSha256": "4" * 64,
            "challenge": native_receipt["challenge"],
            "dispatchAttemptSha256": digest(native_attempt_raw),
            "dmgSha256": native_assets["dmg"]["sha256"],
            "executableSha256": "5" * 64,
            "inputHashVerification": {
                "algorithm": "BLAKE3-256",
                "implementation": "arc-reviewed-js-blake3-one-chunk-v1",
                "implementationSourceSha256": "a" * 64,
                "maximumInputBytes": 1024,
                "promptHash": native_terminal["inputHash"],
            },
            "inputSha256": native_input_sha,
            "packageEvidence": {
                "controllerPath": TRUTH.MACOS_PACKAGE_CONTROLLER_RELATIVE.as_posix(),
                "controllerSha256": controller_sha,
                "inspectionReceipt": inspection,
                "inspectionReceiptSha256": digest(inspection_raw),
                "provenanceReceipt": provenance,
                "provenanceReceiptSha256": digest(provenance_raw),
                "signatureReceipt": signature_receipt,
                "signatureReceiptSha256": digest(signature_raw),
                "verificationReceipt": verification,
                "verificationReceiptSha256": digest(verification_raw),
            },
            "receipt": native_receipt,
            "receiptSha256": digest(native_receipt_raw),
            "scope": TRUTH.PACKAGED_NATIVE_SCOPE,
        }
        appimage_gate_sha = TRUTH.repository_tool(
            TRUTH.PACKAGED_APPIMAGE_VERIFIER_RELATIVE,
            "fixture AppImage verifier",
        )[1]
        appimage_names = (
            "arc-desktop-linux-x86_64.AppImage",
            "arc-desktop-linux-x86_64.AppImage.sig",
            "arc-node-linux-x86_64",
        )
        appimage_receipt = {
            "release": {
                "assets": {
                    name: {"name": name, **self.assets[name]}
                    for name in appimage_names
                },
                "commit": self.source_sha,
            },
            "result": "passed",
            "schema": TRUTH.PACKAGED_APPIMAGE_HOST_SCHEMA,
        }
        self.packaged_appimage_receipt = self.root / "packaged-appimage" / "receipt.json"
        self.packaged_appimage_receipt.parent.mkdir(mode=0o700)
        write_json(self.packaged_appimage_receipt, appimage_receipt)
        appimage_wrapper = {
            "gatePath": TRUTH.PACKAGED_APPIMAGE_VERIFIER_RELATIVE.as_posix(),
            "gateSha256": appimage_gate_sha,
            "receipt": appimage_receipt,
            "receiptSha256": digest(TRUTH.canonical_json(appimage_receipt)),
            "scope": TRUTH.PACKAGED_APPIMAGE_SCOPE,
        }
        receipt = {
            "appSourceTreeSha256": TRUTH.desktop_app_source_tree_sha256(),
            "blockReceipt": {
                "blockHash": block_hash,
                "blockHeight": 137148,
                "gasUsed": 50_000,
                "index": 1,
                "success": True,
                "txHash": tx_hash,
            },
            "earnings": {
                "address": worker,
                "archiveMode": True,
                "canaryReceipt": earnings_receipt,
                "communityRewardsV1ApprovalCollectionReady": True,
                "communityRewardsV1Enabled": True,
                "communityRewardsV1ProtocolActive": True,
                "confirmedGrossEarningsArc": 5.0,
                "confirmedGrossEarningsBase": 5_000_000_000,
                "confirmedReceiptCount": 2,
                "historyCompleteSinceRecovery": True,
                "historyDomain": TRUTH.DESKTOP_HISTORY_DOMAIN,
                "historyScope": TRUTH.DESKTOP_ARCHIVE_SCOPE,
                "issuanceReadyForWorker": True,
                "projectedDailyArc": None,
                "projectedDailyUnavailableReason": "collecting confirmed receipt history",
                "recoveryEpoch": 1,
                "rewardPerAttestationArc": 2.5,
                "rewardPerAttestationBase": 2_500_000_000,
                "source": TRUTH.DESKTOP_RETAINED_SOURCE,
                "stakeZeroEligible": True,
                "validatorSetCommitment": "0x" + "8" * 64,
                "validatorSetId": 1,
                "workerMinStakeBase": 0,
            },
            "forward": {
                "kind": TRUTH.DESKTOP_FORWARD_KIND,
                "localPort": 9090,
                "rolloutManifestSha256": self.manifest_sha256,
                "sshExecutableSha256": TRUTH.DESKTOP_SSH_EXECUTABLE_SHA256,
                "sshIdentitySha256": TRUTH.DESKTOP_SSH_IDENTITY_SHA256,
                "sshKnownHostsSha256": TRUTH.DESKTOP_SSH_KNOWN_HOSTS_SHA256,
                "validatorHost": TRUTH.DESKTOP_VALIDATOR_HOST,
                "validatorName": TRUTH.DESKTOP_VALIDATOR_NAME,
                "validatorRpcSocket": (
                    f"/run/arc-v3-rpc-{TRUTH.DESKTOP_VALIDATOR_NAME}-"
                    f"{self.manifest_sha256[:16]}/rpc.sock"
                ),
            },
            "frontendConfigSha256": digest(config_raw),
            "packageLockSha256": package_lock_sha,
            "packagedAppImage": appimage_wrapper,
            "packagedNative": native_wrapper,
            "playwrightReportSha256": "9" * 64,
            "pollContract": {"maxPolls": 61, "maxWaitMs": 180_000},
            "repository": TRUTH.REPOSITORY,
            "rewardReceipt": direct,
            "rewardTx": tx_hash,
            "rpcOrigin": "http://127.0.0.1:9090",
            "rpcPort": 9090,
            "runtime": {
                "nodeArch": "arm64",
                "nodeDistributionArchiveSha256": TRUTH.DESKTOP_NODE_ARCHIVE_SHA256,
                "nodeExecutableSha256": TRUTH.DESKTOP_NODE_EXECUTABLE_SHA256,
                "nodePlatform": "darwin",
                "nodeVersion": "v24.20.0",
                "npmCliSha256": TRUTH.DESKTOP_NPM_CLI_SHA256,
                "npmPackageSha256": TRUTH.DESKTOP_NPM_PACKAGE_SHA256,
                "npmVersion": "11.19.0",
            },
            "schema": TRUTH.DESKTOP_LIVE_SCHEMA,
            "sourceCommit": self.source_sha,
            "suite": {
                "durationMs": 1234,
                "fileSha256": suite_hashes,
                "playwrightVersion": lock["packages"]["node_modules/@playwright/test"]["version"],
                "startedAt": "2026-09-06T12:00:00.000Z",
                "testCount": 4,
            },
            "worker": worker,
        }
        self.desktop_live_receipt = self.root / "desktop-live-receipt.json"
        write_json(self.desktop_live_receipt, receipt)

    def args(self, output: Path, **overrides: object) -> argparse.Namespace:
        values: dict[str, object] = {"readme": self.readme, "release_api": self.release_path, "pages_workflow": self.pages_workflow, "pages_run": self.pages_run, "pages_jobs": self.pages_jobs, "pages_api": self.pages_api, "pages_deployments": self.pages_deployments, "pages_statuses": self.pages_statuses, "frontend_config": self.config_path, "deployed_commit": self.deployed_commit, "deployed_sha256sums": self.deployed_sums, "published_workflow": self.published_workflow, "published_run": self.published_run, "published_jobs": self.published_jobs, "published_artifact_metadata": self.published_artifact_metadata, "published_artifact_zip": self.published_zip, "reward_evidence": self.reward_path, "rollout_manifest": self.manifest_path, "desktop_live_receipt": self.desktop_live_receipt, "packaged_appimage_receipt": self.packaged_appimage_receipt, "macos_package_evidence_dir": self.macos_package_evidence_dir, "node": self.node_path, "node_sha256": self.node_sha256, "output_dir": output}
        values.update(overrides)
        return argparse.Namespace(**values)


class PublicTruthTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.fixture = Fixture(self.root)
        self.recovery_calls: list[tuple[Path, Path]] = []
        self.product_calls: list[tuple[Path, Path, Path, str]] = []
        self.appimage_calls: list[tuple[Path, Path]] = []

        def verified(manifest, reward, manifest_raw, reward_raw, temporary_root):
            self.recovery_calls.append((manifest, reward))
            return {"manifestSha256": digest(manifest_raw), "rewardEvidenceSha256": digest(reward_raw), "stdoutSha256": "9" * 64, "verifierPath": TRUTH.RECOVERY_VERIFIER_RELATIVE.as_posix(), "verifierSha256": "8" * 64}

        self.recovery_patch = mock.patch.object(TRUTH, "run_recovery_verify", side_effect=verified)
        self.recovery_patch.start()

        def verified_product(config, reward, config_raw, reward_raw, temporary_root, node, node_sha256):
            self.product_calls.append((config, reward, node, node_sha256))
            return {
                "configSha256": digest(config_raw),
                "nodeSha256": node_sha256,
                "nodeVersion": "v24.20.0",
                "readerSha256": {
                    "dashboard/app.js": "3" * 64,
                    "explorer/app.js": "4" * 64,
                    "shared/frontend/arc-network.js": "5" * 64,
                },
                "rewardEvidenceSha256": digest(reward_raw),
                "stdoutSha256": "7" * 64,
                "verifierPath": TRUTH.PRODUCT_SURFACE_VERIFIER_RELATIVE.as_posix(),
                "verifierSha256": "6" * 64,
            }

        self.product_patch = mock.patch.object(
            TRUTH, "run_product_surface_verify", side_effect=verified_product
        )
        self.product_patch.start()

        def verified_appimage(receipt_path, binding_path):
            self.appimage_calls.append((receipt_path, binding_path))
            raw = receipt_path.read_bytes()
            return {
                "receipt": json.loads(raw),
                "receiptSha256": digest(raw),
                "stdoutSha256": "b" * 64,
                "verifierPath": TRUTH.PACKAGED_APPIMAGE_VERIFIER_RELATIVE.as_posix(),
                "verifierSha256": TRUTH.repository_tool(
                    TRUTH.PACKAGED_APPIMAGE_VERIFIER_RELATIVE,
                    "fixture AppImage verifier",
                )[1],
            }

        self.appimage_patch = mock.patch.object(
            TRUTH, "run_packaged_appimage_verify", side_effect=verified_appimage
        )
        self.appimage_patch.start()

    def tearDown(self) -> None:
        self.appimage_patch.stop()
        self.product_patch.stop()
        self.recovery_patch.stop()
        self.temporary.cleanup()

    def test_network_accepts_prefixed_manifest_hashes_and_non_nyc_canonical_source(self) -> None:
        manifest = copy.deepcopy(self.fixture.manifest)
        config = copy.deepcopy(self.fixture.config)
        manifest["chain"]["source_block_hash"] = "0x" + manifest["chain"][
            "source_block_hash"
        ]
        manifest["chain"]["source_state_root"] = "0x" + manifest["chain"][
            "source_state_root"
        ]
        manifest["chain"]["canonical_source"]["node"] = "lax"
        config["checkpoint"]["legacySourceId"] = "v3-lax"
        config["checkpoint"]["v3SourceId"] = "v3-lax"

        checkpoint = TRUTH.validate_network(config, self.fixture.source_sha, manifest)
        self.assertEqual(checkpoint["legacySourceId"], "v3-lax")

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
        self.assertEqual(
            self.product_calls,
            [(
                self.fixture.config_path,
                self.fixture.reward_path,
                self.fixture.node_path,
                self.fixture.node_sha256,
            )],
        )
        self.assertEqual(len(self.appimage_calls), 1)
        self.assertEqual(
            self.appimage_calls[0][0], self.fixture.packaged_appimage_receipt
        )
        self.assertEqual(
            acceptance["productSurfaces"]["verifierPath"],
            TRUTH.PRODUCT_SURFACE_VERIFIER_RELATIVE.as_posix(),
        )
        desktop_live = acceptance["productSurfaces"]["desktopLive"]
        self.assertEqual(
            desktop_live["generatorPath"],
            TRUTH.DESKTOP_LIVE_GENERATOR_RELATIVE.as_posix(),
        )
        self.assertEqual(desktop_live["receipt"]["rewardTx"], "0x" + "d" * 64)
        self.assertEqual(desktop_live["receipt"]["worker"], "0x" + "c" * 64)
        self.assertEqual(
            desktop_live["receipt"]["pollContract"],
            {"maxPolls": 61, "maxWaitMs": 180_000},
        )
        self.assertEqual(
            desktop_live["receipt"]["packagedAppImage"]["scope"],
            TRUTH.PACKAGED_APPIMAGE_SCOPE,
        )
        self.assertEqual(
            desktop_live["receipt"]["packagedNative"]["scope"],
            TRUTH.PACKAGED_NATIVE_SCOPE,
        )
        self.assertEqual(
            desktop_live["macosPackageVerification"]["truthScope"],
            TRUTH.MACOS_PACKAGE_TRUTH_SCOPE,
        )
        self.assertEqual(
            desktop_live["macosPackageVerification"]["verificationReceiptSha256"],
            digest(
                (
                    self.fixture.macos_package_evidence_dir
                    / "MACOS-PACKAGE-PROVENANCE-VERIFICATION.json"
                ).read_bytes()
            ),
        )

    def test_output_directory_entry_and_contents_are_fsynced(self) -> None:
        output = self.root / "durable-output"
        original_fsync = os.fsync
        synced: list[tuple[int, int, int]] = []

        def record_fsync(descriptor: int) -> None:
            identity = os.fstat(descriptor)
            synced.append((identity.st_dev, identity.st_ino, identity.st_mode))
            original_fsync(descriptor)

        with mock.patch.object(TRUTH.os, "fsync", side_effect=record_fsync):
            TRUTH.build(self.fixture.args(output))

        parent = output.parent.stat()
        child = output.stat()
        synced_directories = {
            (device, inode)
            for device, inode, mode in synced
            if stat.S_ISDIR(mode)
        }
        self.assertIn((child.st_dev, child.st_ino), synced_directories)
        self.assertIn((parent.st_dev, parent.st_ino), synced_directories)

    def test_rejects_missing_or_mutated_packaged_runtime_evidence(self) -> None:
        original = json.loads(self.fixture.desktop_live_receipt.read_text())
        mutations = (
            ("missing AppImage", lambda value: value.pop("packagedAppImage")),
            (
                "AppImage receipt mismatch",
                lambda value: value["packagedAppImage"]["receipt"].update(
                    {"result": "failed"}
                ),
            ),
            (
                "native WebView",
                lambda value: value["packagedNative"]["receipt"]["runtime"].update(
                    {"webviewsCreated": 1}
                ),
            ),
            (
                "native provisional tx",
                lambda value: value["packagedNative"]["receipt"]["dispatch"][
                    "result"
                ]["settlement"].update({"txHash": "0x" + "e" * 64}),
            ),
            (
                "native prompt hash",
                lambda value: value["packagedNative"]["inputHashVerification"].update(
                    {"promptHash": "0x" + "f" * 64}
                ),
            ),
        )
        for name, mutate in mutations:
            value = copy.deepcopy(original)
            mutate(value)
            path = self.root / f"bad-packaged-{name.replace(' ', '-')}.json"
            write_json(path, value)
            with self.subTest(name=name), self.assertRaises(TRUTH.TruthError):
                TRUTH.build(
                    self.fixture.args(
                        self.root / f"bad-packaged-{name.replace(' ', '-')}-out",
                        desktop_live_receipt=path,
                    )
                )

    def test_rejects_missing_mutable_or_substituted_macos_package_evidence(self) -> None:
        missing = self.root / "macos-evidence-missing"
        shutil.copytree(self.fixture.macos_package_evidence_dir, missing)
        (missing / "MACOS-PACKAGE-INSPECTION.json").unlink()
        with self.assertRaisesRegex(TRUTH.TruthError, "exact receipt directory"):
            TRUTH.build(
                self.fixture.args(
                    self.root / "macos-missing-out",
                    macos_package_evidence_dir=missing,
                )
            )

        mutable = self.root / "macos-evidence-mutable"
        shutil.copytree(self.fixture.macos_package_evidence_dir, mutable)
        (mutable / "MACOS-UPDATER-SIGNATURE.json").chmod(0o600)
        with self.assertRaisesRegex(TRUTH.TruthError, "mode 0400"):
            TRUTH.build(
                self.fixture.args(
                    self.root / "macos-mutable-out",
                    macos_package_evidence_dir=mutable,
                )
            )

        mutations = (
            (
                "signature",
                "MACOS-UPDATER-SIGNATURE.json",
                lambda value: value.update({"verified": False}),
            ),
            (
                "inspection",
                "MACOS-PACKAGE-INSPECTION.json",
                lambda value: value["extraction"]["safety"].update(
                    {"symlinksResolvedInsideBundle": False}
                ),
            ),
            (
                "provenance",
                "MACOS-PACKAGE-PROVENANCE.json",
                lambda value: value["truthScope"].update(
                    {"appleDeveloperIdSigned": True}
                ),
            ),
            (
                "verification",
                "MACOS-PACKAGE-PROVENANCE-VERIFICATION.json",
                lambda value: value.update({"verified": False}),
            ),
            (
                "attempt",
                "MACOS-NATIVE-CONTROLLER-ATTEMPT.json",
                lambda value: value.update({"retryPermitted": True}),
            ),
        )
        for label, filename, mutate in mutations:
            copied = self.root / f"macos-evidence-{label}"
            shutil.copytree(self.fixture.macos_package_evidence_dir, copied)
            path = copied / filename
            path.chmod(0o600)
            value = json.loads(path.read_text())
            mutate(value)
            write_json(path, value)
            path.chmod(0o400)
            with self.subTest(label=label), self.assertRaises(TRUTH.TruthError):
                TRUTH.build(
                    self.fixture.args(
                        self.root / f"macos-{label}-out",
                        macos_package_evidence_dir=copied,
                    )
                )

    def test_macos_mount_detach_and_root_inventory_fail_closed(self) -> None:
        inspection = json.loads(
            (self.fixture.macos_package_evidence_dir / "MACOS-PACKAGE-INSPECTION.json").read_text()
        )
        attach = inspection["dmg"]["attach"]
        TRUTH._macos_mount(attach, "fixture mount", expected_basename="inspect-dmg-mount")
        for label, mutate in (
            ("tools", lambda value: value.__setitem__("tools", {})),
            ("filesystem", lambda value: value.__setitem__("filesystem", "")),
        ):
            changed = copy.deepcopy(attach)
            mutate(changed)
            with self.subTest(label=label), self.assertRaises(TRUTH.TruthError):
                TRUTH._macos_mount(
                    changed, "fixture mount", expected_basename="inspect-dmg-mount"
                )

        detach = inspection["dmg"]["detach"]
        TRUTH._macos_detach(detach, "fixture detach")
        changed_detach = copy.deepcopy(detach)
        changed_detach.pop("postDetachInfoPlistSha256")
        with self.assertRaises(TRUTH.TruthError):
            TRUTH._macos_detach(changed_detach, "fixture detach")

        inventory = inspection["dmg"]["rootInventory"]
        TRUTH._macos_root_inventory(inventory, "fixture inventory")
        with self.assertRaises(TRUTH.TruthError):
            TRUTH._macos_root_inventory([], "fixture inventory")

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

    def test_live_product_surface_verifier_failure_creates_no_output(self) -> None:
        self.product_patch.stop()
        output = self.root / "product-verify-failed-output"
        with mock.patch.object(
            TRUTH,
            "run_product_surface_verify",
            side_effect=TRUTH.TruthError("canary is absent from explorer"),
        ):
            with self.assertRaisesRegex(TRUTH.TruthError, "absent from explorer"):
                TRUTH.build(self.fixture.args(output))
        self.product_patch.start()
        self.assertFalse(output.exists())

    def test_desktop_live_receipt_rejects_malformed_or_mismatched_canary(self) -> None:
        base = json.loads(self.fixture.desktop_live_receipt.read_text())
        mutations = {
            "missing-key": lambda value: value["rewardReceipt"].pop("evidence_source"),
            "wrong-tx": lambda value: value["rewardReceipt"].__setitem__("tx_hash", "0x" + "a" * 64),
            "wrong-job": lambda value: value["rewardReceipt"].__setitem__("job_id", "0x" + "a" * 64),
            "wrong-worker": lambda value: value["rewardReceipt"].__setitem__("worker", "0x" + "a" * 64),
            "wrong-status": lambda value: value["rewardReceipt"].__setitem__("status", "mined_failed"),
            "wrong-reward": lambda value: value["rewardReceipt"].__setitem__("reward_base", 1),
        }
        for name, mutate in mutations.items():
            with self.subTest(name=name):
                value = copy.deepcopy(base)
                mutate(value)
                path = self.root / f"desktop-live-{name}.json"
                write_json(path, value)
                output = self.root / f"desktop-live-{name}-output"
                with self.assertRaises(TRUTH.TruthError):
                    TRUTH.build(
                        self.fixture.args(output, desktop_live_receipt=path)
                    )
                self.assertFalse(output.exists())

    def test_desktop_live_receipt_rejects_dirty_source_bindings(self) -> None:
        base = json.loads(self.fixture.desktop_live_receipt.read_text())
        mutations = {
            "source": ("sourceCommit", "0" * 40),
            "config": ("frontendConfigSha256", "0" * 64),
            "app": ("appSourceTreeSha256", "0" * 64),
            "lock": ("packageLockSha256", "0" * 64),
        }
        for name, (field, replacement) in mutations.items():
            with self.subTest(name=name):
                value = copy.deepcopy(base)
                value[field] = replacement
                path = self.root / f"desktop-live-dirty-{name}.json"
                write_json(path, value)
                output = self.root / f"desktop-live-dirty-{name}-output"
                with self.assertRaises(TRUTH.TruthError):
                    TRUTH.build(
                        self.fixture.args(output, desktop_live_receipt=path)
                    )
                self.assertFalse(output.exists())

        suite = copy.deepcopy(base)
        suite["suite"]["fileSha256"]["desktop/tests/live.spec.ts"] = "0" * 64
        suite_path = self.root / "desktop-live-dirty-suite.json"
        write_json(suite_path, suite)
        with self.assertRaisesRegex(TRUTH.TruthError, "differs from protected source"):
            TRUTH.build(
                self.fixture.args(
                    self.root / "desktop-live-dirty-suite-output",
                    desktop_live_receipt=suite_path,
                )
            )

    def test_desktop_live_receipt_rejects_wrong_runtime_or_forward(self) -> None:
        base = json.loads(self.fixture.desktop_live_receipt.read_text())
        mutations = {
            "runtime-node": lambda value: value["runtime"].__setitem__(
                "nodeExecutableSha256", "0" * 64
            ),
            "runtime-npm": lambda value: value["runtime"].__setitem__(
                "npmVersion", "11.18.0"
            ),
            "forward-kind": lambda value: value["forward"].__setitem__(
                "kind", "plain-loopback"
            ),
            "forward-host": lambda value: value["forward"].__setitem__(
                "validatorHost", "149.28.32.76"
            ),
            "forward-rollout": lambda value: value["forward"].__setitem__(
                "rolloutManifestSha256", "0" * 64
            ),
            "forward-socket": lambda value: value["forward"].__setitem__(
                "validatorRpcSocket", "/run/arc-v3-rpc-lax-unbound/rpc.sock"
            ),
        }
        for name, mutate in mutations.items():
            with self.subTest(name=name):
                value = copy.deepcopy(base)
                mutate(value)
                path = self.root / f"desktop-live-{name}.json"
                write_json(path, value)
                output = self.root / f"desktop-live-{name}-output"
                with self.assertRaises(TRUTH.TruthError):
                    TRUTH.build(
                        self.fixture.args(output, desktop_live_receipt=path)
                    )
                self.assertFalse(output.exists())

    def test_product_surface_verifier_rejects_unreviewed_node_bytes(self) -> None:
        self.product_patch.stop()
        self.fixture.node_path.chmod(0o500)
        try:
            with tempfile.TemporaryDirectory() as temporary:
                with self.assertRaisesRegex(
                    TRUTH.TruthError, "differs from its reviewed SHA-256"
                ):
                    TRUTH.run_product_surface_verify(
                        self.fixture.config_path,
                        self.fixture.reward_path,
                        self.fixture.config_path.read_bytes(),
                        self.fixture.reward_path.read_bytes(),
                        Path(temporary),
                        self.fixture.node_path,
                        "0" * 64,
                    )
        finally:
            self.product_patch.start()

    def test_rejects_mutable_release_duplicate_reward_and_existing_output(self) -> None:
        release = copy.deepcopy(self.fixture.release)
        release["immutable"] = False
        release_path = self.root / "mutable-release.json"
        write_json(release_path, release, canonical=False)
        with self.assertRaisesRegex(TRUTH.TruthError, "immutable"):
            TRUTH.build(self.fixture.args(self.root / "mutable-output", release_api=release_path))
        duplicate_asset_ids = copy.deepcopy(self.fixture.release)
        duplicate_asset_ids["assets"][1]["id"] = duplicate_asset_ids["assets"][0]["id"]
        duplicate_asset_ids_path = self.root / "duplicate-asset-ids.json"
        write_json(duplicate_asset_ids_path, duplicate_asset_ids, canonical=False)
        with self.assertRaisesRegex(TRUTH.TruthError, "immutable uploaded digest contract"):
            TRUTH.build(
                self.fixture.args(
                    self.root / "duplicate-asset-ids-output",
                    release_api=duplicate_asset_ids_path,
                )
            )
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

    def test_rejects_duplicate_packaged_appimage_asset_ids(self) -> None:
        external = json.loads(self.fixture.packaged_appimage_receipt.read_text())
        names = sorted(external["release"]["assets"])
        external["release"]["assets"][names[1]]["id"] = external["release"]["assets"][names[0]]["id"]
        external_path = self.root / "duplicate-appimage-assets" / "receipt.json"
        external_path.parent.mkdir(mode=0o700)
        external_raw = write_json(external_path, external)

        desktop = json.loads(self.fixture.desktop_live_receipt.read_text())
        desktop["packagedAppImage"]["receipt"] = external
        desktop["packagedAppImage"]["receiptSha256"] = digest(external_raw)
        desktop_path = self.root / "duplicate-appimage-desktop.json"
        write_json(desktop_path, desktop)
        with self.assertRaisesRegex(TRUTH.TruthError, "asset IDs are not distinct"):
            TRUTH.build(
                self.fixture.args(
                    self.root / "duplicate-appimage-output",
                    packaged_appimage_receipt=external_path,
                    desktop_live_receipt=desktop_path,
                )
            )

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
