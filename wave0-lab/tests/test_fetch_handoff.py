"""Tests for wave0-lab/fetch_handoff.py with a synthetic artifact and a fake gh (THROWAWAY LAB FILE)."""
from __future__ import annotations

import copy
import hashlib
import io
import json
import tempfile
import unittest
import zipfile
from pathlib import Path

import _paths  # noqa: F401
import fetch_handoff as fh


def sha(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def build_artifact(mutate=None):
    """Return (config, zip_bytes, record) for a synthetic nine-member handoff artifact."""
    launchers = {asset: (asset + " bytes\n").encode() * 50 for asset in fh.ASSETS}
    sums = "".join(f"{sha(data)}  {asset}\n" for asset, data in sorted(launchers.items()))
    members = {
        "RELEASE-NOTES.md": b"notes\n",
        "legacy-bridge-provenance.json": json.dumps({"schema": fh.PROVENANCE_SCHEMA, "eligible_for_latest": False}).encode(),
        "v0.7.12/SHA256SUMS": sums.encode(),
        "v0.7.12/latest.json": b"{}\n",
    }
    for asset, data in launchers.items():
        members[f"v0.7.12/{asset}"] = data
    if mutate:
        mutate(members)
    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, "w") as archive:
        for name, data in members.items():
            archive.writestr(name, data)
    blob = buffer.getvalue()
    config = {
        "repository": "FerrumVir/arc-chain",
        "base_commit": "b" * 40,
        "handoff": {
            "run_id": 11,
            "artifact_id": 22,
            "artifact_name": "legacy-bridge-release-handoff",
            "artifact_digest": "sha256:" + sha(blob),
            "artifact_size": len(blob),
            "tag": "v0.7.12",
            "launchers": {asset: sha(data) for asset, data in launchers.items()},
        },
    }
    record = {
        "id": 22,
        "name": "legacy-bridge-release-handoff",
        "expired": False,
        "digest": "sha256:" + sha(blob),
        "size_in_bytes": len(blob),
        "workflow_run": {"id": 11, "head_sha": "b" * 40},
    }
    return config, blob, record


def fake_gh(record, blob):
    def run(args):
        path = args[1]
        if path.endswith("/zip"):
            return blob
        return json.dumps(record).encode()

    return run


class FetchHandoffTests(unittest.TestCase):
    def fetch(self, config, blob, record):
        with tempfile.TemporaryDirectory() as tmp:
            return fh.fetch(config, Path(tmp) / "out", gh=fake_gh(record, blob)), tmp

    def test_success_extracts_and_records(self):
        config, blob, record = build_artifact()
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / "out"
            verified = fh.fetch(config, out, gh=fake_gh(record, blob))
            self.assertEqual(verified["launchers"], config["handoff"]["launchers"])
            self.assertTrue((out / "v0.7.12" / "arc-node-linux-x86_64").is_file())
            self.assertTrue((out / "handoff-verified.json").is_file())

    def refuse(self, config, blob, record, needle):
        with tempfile.TemporaryDirectory() as tmp:
            with self.assertRaises(ValueError) as caught:
                fh.fetch(config, Path(tmp) / "out", gh=fake_gh(record, blob))
            self.assertIn(needle, str(caught.exception))
            self.assertFalse((Path(tmp) / "out" / "v0.7.12").exists(), "nothing may be extracted on a refusal")

    def test_digest_mismatch(self):
        config, blob, record = build_artifact()
        config["handoff"]["artifact_digest"] = "sha256:" + "0" * 64
        self.refuse(config, blob, record, "artifact digest")

    def test_downloaded_bytes_differ_from_the_record(self):
        config, blob, record = build_artifact()
        self.refuse(config, blob + b"x", record, "downloaded zip hashes")

    def test_expired(self):
        config, blob, record = build_artifact()
        record["expired"] = True
        self.refuse(config, blob, record, "expired")

    def test_wrong_run(self):
        config, blob, record = build_artifact()
        record["workflow_run"]["id"] = 12
        self.refuse(config, blob, record, "pinned run")

    def test_wrong_head(self):
        config, blob, record = build_artifact()
        record["workflow_run"]["head_sha"] = "c" * 40
        self.refuse(config, blob, record, "base commit")

    def test_wrong_name(self):
        config, blob, record = build_artifact()
        record["name"] = "other"
        self.refuse(config, blob, record, "artifact name")

    def test_extra_member(self):
        config, blob, record = build_artifact(lambda members: members.update({"extra.txt": b"x"}))
        self.refuse(config, blob, record, "extra")

    def test_missing_member(self):
        config, blob, record = build_artifact(lambda members: members.pop("v0.7.12/latest.json"))
        self.refuse(config, blob, record, "missing")

    def test_launcher_differs_from_config(self):
        config, blob, record = build_artifact()
        config["handoff"]["launchers"]["arc-node-macos-arm64"] = "0" * 64
        self.refuse(config, blob, record, "SHA256SUMS differs")

    def test_sums_file_lies(self):
        def lie(members):
            members["v0.7.12/arc-node-macos-arm64"] = b"tampered"
        config, blob, record = build_artifact(lie)
        self.refuse(config, blob, record, "hashes to")

    def test_provenance_eligible(self):
        def eligible(members):
            members["legacy-bridge-provenance.json"] = json.dumps({"schema": fh.PROVENANCE_SCHEMA, "eligible_for_latest": True}).encode()
        config, blob, record = build_artifact(eligible)
        self.refuse(config, blob, record, "eligible_for_latest")

    def test_provenance_schema(self):
        def schema(members):
            members["legacy-bridge-provenance.json"] = json.dumps({"schema": "other", "eligible_for_latest": False}).encode()
        config, blob, record = build_artifact(schema)
        self.refuse(config, blob, record, "provenance schema")

    def test_unsafe_member_paths(self):
        for name in ("../escape", "/abs/path", "v0.7.12/../../x"):
            with self.subTest(name):
                config, blob, record = build_artifact(lambda members, n=name: members.update({n: b"x"}))
                self.refuse(config, blob, record, "unsafe member path")

    def test_sha256sums_parser(self):
        self.assertEqual(fh.parse_sums("a" * 64 + "  file\n"), {"file": "a" * 64})
        with self.assertRaises(ValueError):
            fh.parse_sums("short  file\n")
        with self.assertRaises(ValueError):
            fh.parse_sums(("a" * 64 + "  file\n") * 2)

    def test_run_gh_refuses_non_get(self):
        for args in (["api", "x", "-X", "POST"], ["api", "x", "-f", "a=b"], ["api", "x", "--method", "DELETE"], ["repo", "delete"]):
            with self.subTest(args):
                with self.assertRaises(SystemExit):
                    fh.run_gh(args)


if __name__ == "__main__":
    unittest.main()
