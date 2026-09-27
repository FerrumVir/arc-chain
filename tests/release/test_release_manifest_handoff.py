#!/usr/bin/env python3
"""Adversarial tests for isolated release-manifest signing handoffs."""

from __future__ import annotations

import hashlib
import importlib.util
import os
import subprocess
import tempfile
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[2]
HELPER = REPO_ROOT / "scripts" / "release" / "release-manifest-handoff.py"
REPOSITORY = "FerrumVir/arc-chain"
COMMIT = "a" * 40
TAG = "v0.8.0"
RUN_ID = 2468
ATTEMPT = 3

spec = importlib.util.spec_from_file_location("release_handoff", HELPER)
assert spec is not None and spec.loader is not None
HANDOFF = importlib.util.module_from_spec(spec)
spec.loader.exec_module(HANDOFF)


def digest(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


class ReleaseManifestHandoffTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory(prefix="arc-release-handoff-test.")
        self.root = Path(self.temporary.name)

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def invoke(self, command: str, *extra: str, succeeds: bool = True):
        result = subprocess.run(
            [
                "python3", str(HELPER), command,
                "--repository", REPOSITORY,
                "--commit", COMMIT,
                "--tag", TAG,
                "--run-id", str(RUN_ID),
                "--run-attempt", str(ATTEMPT),
                *extra,
            ],
            cwd=REPO_ROOT,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        if succeeds and result.returncode != 0:
            self.fail(result.stderr)
        if not succeeds and result.returncode == 0:
            self.fail("helper accepted an invalid release handoff")
        return result

    def release_files(self, name: str, *, sealed: bool = False, profile: str = HANDOFF.PROFILE_CUTOVER_V1) -> Path:
        root = self.root / name
        root.mkdir()
        files = HANDOFF.profile_files(profile)
        for index, filename in enumerate(files, start=1):
            if filename != "SHA256SUMS":
                (root / filename).write_bytes(f"{filename}:{index}\n".encode())
        records = [
            "# ARC release manifest v1",
            f"# repository={REPOSITORY}",
            f"# tag={TAG}",
            f"# commit={COMMIT}",
        ]
        for filename in sorted(set(files) - {"SHA256SUMS"}):
            records.append(f"{digest(root / filename)}  {filename}")
        (root / "SHA256SUMS").write_text("\n".join(records) + "\n", encoding="utf-8")
        if sealed:
            (root / "SHA256SUMS.sig").write_bytes(b"fixture signature\n")
        return root

    def stage(self, name: str, *, sealed: bool = False, profile: str = HANDOFF.PROFILE_CUTOVER_V1) -> tuple[Path, dict[str, str]]:
        source = self.release_files(f"{name}-source", sealed=sealed, profile=profile)
        stage = self.root / f"{name}-stage"
        output = self.root / f"{name}.out"
        arguments = [
            "--source-dir", str(source),
            "--stage-dir", str(stage),
            "--github-output", str(output),
        ]
        if sealed:
            arguments.insert(0, "--sealed")
        if profile != HANDOFF.PROFILE_CUTOVER_V1:
            arguments.extend(("--profile", profile))
        self.invoke("stage", *arguments)
        outputs = dict(
            line.split("=", 1) for line in output.read_text(encoding="utf-8").splitlines()
        )
        return stage, outputs

    def test_unsigned_and_sealed_round_trip_are_hash_and_run_bound(self) -> None:
        for sealed in (False, True):
            with self.subTest(sealed=sealed):
                stage, outputs = self.stage(f"roundtrip-{sealed}", sealed=sealed)
                arguments = [
                    "--handoff-dir", str(stage),
                    "--expected-metadata-sha", outputs["metadata_sha"],
                ]
                if sealed:
                    arguments.insert(0, "--sealed")
                self.invoke("verify", *arguments)
                kind = "sealed" if sealed else "unsigned"
                self.assertEqual(
                    outputs["artifact_name"],
                    f"arc-release-{kind}-handoff-{COMMIT}-{RUN_ID}-{ATTEMPT}-{outputs['metadata_sha']}",
                )

    def test_update_profile_v2_round_trip_is_disjoint_and_profile_bound(self) -> None:
        stage, outputs = self.stage("update-profile", profile=HANDOFF.PROFILE_EXISTING_UPDATE_V1)
        metadata = __import__("json").loads((stage / HANDOFF.HANDOFF_NAME).read_text())
        self.assertEqual(metadata["schema"], HANDOFF.SCHEMA_V2)
        self.assertEqual(metadata["profile"], HANDOFF.PROFILE_EXISTING_UPDATE_V1)
        members = {p.name for p in (stage / "release-files").iterdir()}
        self.assertIn("arc-existing-chain-update-attestation.json", members)
        self.assertFalse(members & {"arc-cutover-policy.json", "arc-recovery-checkpoint-descriptor.json",
                                    "arc-legacy-maintenance-boundary.json"})
        self.invoke("verify", "--profile", HANDOFF.PROFILE_EXISTING_UPDATE_V1,
                    "--handoff-dir", str(stage), "--expected-metadata-sha", outputs["metadata_sha"])
        self.invoke("verify", "--handoff-dir", str(stage),
                    "--expected-metadata-sha", outputs["metadata_sha"], succeeds=False)

    def test_update_profile_rejects_mixed_assets_and_schema_downgrade(self) -> None:
        source = self.release_files("update-mixed", profile=HANDOFF.PROFILE_EXISTING_UPDATE_V1)
        (source / "arc-cutover-policy.json").write_bytes(b"cutover mixed in\n")
        self.invoke("stage", "--profile", HANDOFF.PROFILE_EXISTING_UPDATE_V1,
                    "--source-dir", str(source), "--stage-dir", str(self.root / "update-mixed-stage"),
                    succeeds=False)

        stage, outputs = self.stage("update-downgrade", profile=HANDOFF.PROFILE_EXISTING_UPDATE_V1)
        metadata_path = stage / HANDOFF.HANDOFF_NAME
        metadata_path.chmod(0o600)
        metadata = __import__("json").loads(metadata_path.read_text())
        metadata["schema"] = HANDOFF.SCHEMA_V1
        metadata.pop("profile")
        raw = HANDOFF.canonical_json(metadata)
        metadata_path.write_bytes(raw)
        self.invoke("verify", "--profile", HANDOFF.PROFILE_EXISTING_UPDATE_V1,
                    "--handoff-dir", str(stage), "--expected-metadata-sha", hashlib.sha256(raw).hexdigest(),
                    succeeds=False)

    def test_stage_rejects_missing_extra_symlink_and_cross_signature_members(self) -> None:
        for mutation in ("missing", "extra", "symlink", "signature"):
            with self.subTest(mutation=mutation):
                source = self.release_files(f"invalid-{mutation}")
                if mutation == "missing":
                    (source / "genesis.toml").unlink()
                elif mutation == "extra":
                    (source / "extra.bin").write_bytes(b"extra")
                elif mutation == "symlink":
                    (source / "genesis.toml").unlink()
                    os.symlink(source / "latest.json", source / "genesis.toml")
                else:
                    (source / "SHA256SUMS.sig").write_bytes(b"wrong phase")
                self.invoke(
                    "stage",
                    "--source-dir", str(source),
                    "--stage-dir", str(self.root / f"invalid-{mutation}-stage"),
                    succeeds=False,
                )

    def test_verify_rejects_tamper_metadata_substitution_and_unexpected_output(self) -> None:
        stage, outputs = self.stage("tamper")
        (stage / "release-files" / "latest.json").write_bytes(b"tampered\n")
        self.invoke(
            "verify",
            "--handoff-dir", str(stage),
            "--expected-metadata-sha", outputs["metadata_sha"],
            succeeds=False,
        )

        stage, _ = self.stage("metadata")
        self.invoke(
            "verify",
            "--handoff-dir", str(stage),
            "--expected-metadata-sha", "f" * 64,
            succeeds=False,
        )

        stage, outputs = self.stage("extra-output", sealed=True)
        (stage / "release-files" / "background-output").write_bytes(b"blocked")
        self.invoke(
            "verify", "--sealed",
            "--handoff-dir", str(stage),
            "--expected-metadata-sha", outputs["metadata_sha"],
            succeeds=False,
        )


if __name__ == "__main__":
    unittest.main(verbosity=2)
