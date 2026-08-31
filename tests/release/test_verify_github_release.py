#!/usr/bin/env python3
"""Adversarial tests for server-side GitHub release verification."""

from __future__ import annotations

import hashlib
import json
import subprocess
import tempfile
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[2]
VERIFIER = REPO_ROOT / "scripts" / "release" / "verify-github-release.py"
TAG = "v0.8.0"
COMMIT = "a" * 40


class ReleaseVerificationTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.assets = self.root / "release-files"
        self.assets.mkdir()
        (self.assets / "arc-node-linux-x86_64").write_bytes(b"exact-node-bytes\n")
        (self.assets / "SHA256SUMS").write_bytes(b"signed-manifest\n")
        self.release_json = self.root / "release.json"
        self.release = self.make_release(draft=True, immutable=False)

    def make_release(self, *, draft: bool, immutable: bool) -> dict[str, object]:
        assets = []
        for path in sorted(self.assets.iterdir()):
            assets.append(
                {
                    "name": path.name,
                    "state": "uploaded",
                    "size": path.stat().st_size,
                    "digest": f"sha256:{hashlib.sha256(path.read_bytes()).hexdigest()}",
                    "uploader": {"login": "github-actions[bot]"},
                }
            )
        return {
            "id": 12345,
            "tag_name": TAG,
            "target_commitish": COMMIT,
            "draft": draft,
            "prerelease": False,
            "immutable": immutable,
            "author": {"login": "github-actions[bot]"},
            "assets": assets,
        }

    def verify(
        self,
        *,
        draft: bool = True,
        immutable: bool = False,
        expected_id: int | None = None,
    ) -> subprocess.CompletedProcess[str]:
        self.release_json.write_text(json.dumps(self.release), encoding="utf-8")
        command = [
            "python3",
            str(VERIFIER),
            "--release-json",
            str(self.release_json),
            "--asset-directory",
            str(self.assets),
            "--tag",
            TAG,
            "--commit",
            COMMIT,
            "--draft",
            str(draft).lower(),
            "--immutable",
            str(immutable).lower(),
        ]
        if expected_id is not None:
            command.extend(("--expected-id", str(expected_id)))
        return subprocess.run(command, text=True, capture_output=True, check=False)

    def assert_rejected(self, message: str) -> None:
        result = self.verify()
        self.assertNotEqual(result.returncode, 0, result)
        self.assertIn(message, result.stderr)

    def test_accepts_exact_hidden_draft_and_exact_published_immutable_release(self) -> None:
        draft = self.verify(expected_id=12345)
        self.assertEqual(draft.returncode, 0, draft.stderr)

        self.release = self.make_release(draft=False, immutable=True)
        published = self.verify(draft=False, immutable=True, expected_id=12345)
        self.assertEqual(published.returncode, 0, published.stderr)

    def test_rejects_asset_digest_size_state_and_uploader_tampering(self) -> None:
        first = self.release["assets"][0]  # type: ignore[index]
        for field, value, message in (
            ("digest", "sha256:" + "0" * 64, "digest mismatch"),
            ("size", 1, "size mismatch"),
            ("state", "new", "state mismatch"),
            ("uploader", {"login": "attacker"}, "uploader login mismatch"),
        ):
            original = first[field]
            first[field] = value
            self.assert_rejected(message)
            first[field] = original

    def test_rejects_extra_duplicate_or_missing_assets(self) -> None:
        assets = self.release["assets"]  # type: ignore[assignment]
        removed = assets.pop()  # type: ignore[union-attr]
        self.assert_rejected("asset name set mismatch")
        assets.append(removed)  # type: ignore[union-attr]
        assets.append(dict(removed))  # type: ignore[union-attr]
        self.assert_rejected("duplicate remote release asset")

    def test_rejects_wrong_identity_commit_state_or_release_id(self) -> None:
        mutations = (
            ("target_commitish", "b" * 40, "release target mismatch"),
            ("author", {"login": "maintainer"}, "release author login mismatch"),
            ("draft", False, "release draft state mismatch"),
            ("immutable", True, "release immutable state mismatch"),
        )
        for field, value, message in mutations:
            original = self.release[field]
            self.release[field] = value
            self.assert_rejected(message)
            self.release[field] = original

        wrong_id = self.verify(expected_id=99999)
        self.assertNotEqual(wrong_id.returncode, 0)
        self.assertIn("release id mismatch", wrong_id.stderr)

    def test_rejects_empty_files_and_nonregular_members(self) -> None:
        empty = self.assets / "empty"
        empty.touch()
        self.assert_rejected("local release asset is empty")
        empty.unlink()

        nested = self.assets / "directory"
        nested.mkdir()
        self.assert_rejected("not a regular file")


if __name__ == "__main__":
    unittest.main(verbosity=2)
