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

    def release_files(self, name: str, *, sealed: bool = False) -> Path:
        root = self.root / name
        root.mkdir()
        for index, filename in enumerate(HANDOFF.BASE_FILES, start=1):
            if filename != "SHA256SUMS":
                (root / filename).write_bytes(f"{filename}:{index}\n".encode())
        records = [
            "# ARC release manifest v1",
            f"# repository={REPOSITORY}",
            f"# tag={TAG}",
            f"# commit={COMMIT}",
        ]
        for filename in sorted(set(HANDOFF.BASE_FILES) - {"SHA256SUMS"}):
            records.append(f"{digest(root / filename)}  {filename}")
        (root / "SHA256SUMS").write_text("\n".join(records) + "\n", encoding="utf-8")
        if sealed:
            (root / "SHA256SUMS.sig").write_bytes(b"fixture signature\n")
        return root

    def stage(self, name: str, *, sealed: bool = False) -> tuple[Path, dict[str, str]]:
        source = self.release_files(f"{name}-source", sealed=sealed)
        stage = self.root / f"{name}-stage"
        output = self.root / f"{name}.out"
        arguments = [
            "--source-dir", str(source),
            "--stage-dir", str(stage),
            "--github-output", str(output),
        ]
        if sealed:
            arguments.insert(0, "--sealed")
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
