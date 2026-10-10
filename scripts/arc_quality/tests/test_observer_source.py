"""Real Git checkout controls: EOL conversion passes; source substitutions fail."""
from pathlib import Path
import subprocess
import tempfile
import unittest

from arc_quality.layer_probe.source_identity import MODEL_PATH, verify_engine_source


class ObserverSourceTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.engine = self.root/'engine'
        self.engine.mkdir()
        self.git('init', '-q')
        self.git('config', 'core.autocrlf', 'true')
        self.source = self.engine/MODEL_PATH
        self.source.parent.mkdir(parents=True)
        self.canonical = b'fn value() -> i32 {\n    17\n}\n'
        self.source.write_bytes(self.canonical)
        self.extra_paths = ('crates/arc-inference/src/modern/mla/ops.rs',
                            'crates/arc-inference/src/canonical_simd.rs',
                            'scripts/arc_mla/make_tiny_kimi_packed.py',
                            'scripts/arc_mla/make_tiny_mla_model.py',
                            'scripts/arc_conformance/mla_moe_reference.py',
                            'scripts/arc_conformance/modern_reference.py')
        for name in self.extra_paths:
            path = self.engine/name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(self.canonical)
        self.git('add', '.')
        self.git('-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid',
                 'commit', '-qm', 'immutable test source')
        self.revision = self.git('rev-parse', 'HEAD').decode().strip()
        self.observer = self.root/'observer.rs'
        self.observer.write_bytes(self.canonical)

    def git(self, *args):
        return subprocess.check_output(['git', *args], cwd=self.engine, stderr=subprocess.PIPE)

    def verify(self):
        return verify_engine_source(self.engine, self.observer, self.revision)

    def test_clean_lf_and_real_crlf_checkout_preserve_blob_identity(self):
        lf = self.verify()
        for name in (MODEL_PATH, *self.extra_paths):
            (self.engine/name).unlink()
        self.git('checkout-index', '-a')
        self.assertEqual(self.source.read_bytes(), self.canonical.replace(b'\n', b'\r\n'))
        self.assertNotEqual(self.source.read_bytes(), self.observer.read_bytes())  # old guard fails
        crlf = self.verify()
        self.assertEqual(crlf['observer_sha256'], crlf['git_blob_sha256'])
        self.assertEqual(lf['git_blob_sha256'], crlf['git_blob_sha256'])
        self.assertNotEqual(crlf['worktree_sha256'], crlf['git_blob_sha256'])
        self.assertEqual(crlf['source_file_count'], 1 + len(self.extra_paths))
        self.assertTrue(all(v['checkout_form'] == 'crlf' for v in crlf['source_files'].values()))

    def test_substantive_unstaged_and_staged_source_mutation_rejected(self):
        self.source.write_bytes(self.canonical.replace(b'17', b'18').replace(b'\n', b'\r\n'))
        with self.assertRaises(subprocess.CalledProcessError):
            self.verify()
        self.git('add', MODEL_PATH)
        with self.assertRaises(subprocess.CalledProcessError):
            self.verify()

    def test_index_hint_cannot_hide_substantive_worktree_mutation(self):
        self.git('update-index', '--assume-unchanged', MODEL_PATH)
        self.source.write_bytes(self.canonical.replace(b'17', b'18'))
        with self.assertRaisesRegex(ValueError, 'beyond checkout line endings'):
            self.verify()

    def test_hidden_nonmodel_and_generator_edits_rejected(self):
        for flag in ('assume-unchanged', 'skip-worktree'):
            for name in self.extra_paths:
                with self.subTest(flag=flag, path=name):
                    path = self.engine/name
                    self.git('update-index', '--'+flag, name)
                    path.write_bytes(self.canonical.replace(b'17', b'18'))
                    # Demonstrate the index-based guard cannot see this change.
                    self.git('diff', '--quiet', self.revision)
                    with self.assertRaisesRegex(ValueError, 'beyond checkout line endings'):
                        self.verify()
                    path.write_bytes(self.canonical)
                    self.git('update-index', '--no-'+flag, name)

    def test_nonmodel_and_generator_staged_edits_rejected(self):
        for name in self.extra_paths:
            with self.subTest(path=name):
                path = self.engine/name
                path.write_bytes(self.canonical.replace(b'17', b'18'))
                self.git('add', name)
                # Restoring the worktree must not hide a staged alteration.
                path.write_bytes(self.canonical)
                with self.assertRaises(subprocess.CalledProcessError):
                    self.verify()
                self.git('add', name)

    def test_hidden_missing_file_and_mixed_eol_rejected(self):
        name = self.extra_paths[0]
        path = self.engine/name
        self.git('update-index', '--assume-unchanged', name)
        path.unlink()
        with self.assertRaisesRegex(ValueError, 'missing/nonregular'):
            self.verify()
        path.write_bytes(self.canonical.replace(b'\n', b'\r\n', 1))
        with self.assertRaisesRegex(ValueError, 'beyond checkout line endings'):
            self.verify()

    def test_observer_mutation_and_observer_crlf_are_not_normalized(self):
        for changed in (self.canonical.replace(b'17', b'18'), self.canonical.replace(b'\n', b'\r\n')):
            self.observer.write_bytes(changed)
            with self.assertRaisesRegex(ValueError, 'pinned engine Git blob'):
                self.verify()

    def test_wrong_revision_and_deleted_source_rejected(self):
        with self.assertRaisesRegex(ValueError, 'exact pin'):
            verify_engine_source(self.engine, self.observer, '0'*40)
        self.source.unlink()
        with self.assertRaises(subprocess.CalledProcessError):
            self.verify()


if __name__ == '__main__':
    unittest.main()
