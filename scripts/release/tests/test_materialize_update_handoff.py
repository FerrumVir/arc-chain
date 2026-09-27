import argparse
import hashlib
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
import zipfile

SCRIPT = Path(__file__).resolve().parents[1] / "materialize-update-handoff.py"
SPEC = importlib.util.spec_from_file_location("update_materialize", SCRIPT)
mod = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(mod)


class UpdateMaterializeTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.download = self.root / "download"
        self.download.mkdir()
        self.commit = "a" * 40
        self.payload = json.dumps({"schema": "arc.existing-recovered-chain-update/v1",
            "release": {"repository": "FerrumVir/arc-chain", "commit": self.commit}}).encode()

    def package(self, entries=None):
        archive = self.download / "artifact.zip"
        with zipfile.ZipFile(archive, "w") as handle:
            for name, payload in entries or [(mod.ASSET, self.payload)]:
                handle.writestr(name, payload)
        return argparse.Namespace(downloads_root=self.download, output_dir=self.root / "out",
            artifact_id=42, artifact_name=f"arc-existing-chain-update-handoff-{self.commit}-123-1",
            artifact_digest="sha256:" + hashlib.sha256(archive.read_bytes()).hexdigest(),
            artifact_size=archive.stat().st_size, commit=self.commit)

    def test_exact_selected_zip_materializes_once(self):
        args = self.package()
        mod.materialize(args)
        self.assertEqual((args.output_dir / mod.ASSET).read_bytes(), self.payload)
        with self.assertRaisesRegex(ValueError, "output must be absent"):
            mod.materialize(args)

    def test_swapped_raw_zip_refused_even_with_same_membership(self):
        args = self.package()
        args.artifact_digest = "sha256:" + "0" * 64
        with self.assertRaisesRegex(ValueError, "immutable digest"):
            mod.materialize(args)

    def test_mixed_profile_and_path_traversal_refused(self):
        for name in ("arc-cutover-policy.json", "../" + mod.ASSET):
            with self.subTest(name=name):
                args = self.package([(mod.ASSET, self.payload), (name, b"{}")])
                with self.assertRaisesRegex(ValueError, "exactly one file"):
                    mod.materialize(args)

    def test_wrong_source_commit_and_cross_profile_name_refused(self):
        args = self.package()
        args.artifact_name = f"arc-recovery-release-handoff-{self.commit}-123-1"
        with self.assertRaisesRegex(ValueError, "name is not"):
            mod.materialize(args)
        args = self.package()
        self.payload = self.payload.replace(self.commit.encode(), b"b" * 40)
        args = self.package()
        with self.assertRaisesRegex(ValueError, "schema/repository/commit"):
            mod.materialize(args)

    def test_symlink_member_refused(self):
        info = zipfile.ZipInfo(mod.ASSET)
        info.external_attr = 0o120777 << 16
        args = self.package([(info, self.payload)])
        with self.assertRaisesRegex(ValueError, "unsafe ZIP entry"):
            mod.materialize(args)


if __name__ == "__main__":
    unittest.main()
