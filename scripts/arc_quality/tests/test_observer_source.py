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
        self.git('add', MODEL_PATH)
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
        self.source.unlink()
        self.git('checkout-index', '-a')
        self.assertEqual(self.source.read_bytes(), self.canonical.replace(b'\n', b'\r\n'))
        self.assertNotEqual(self.source.read_bytes(), self.observer.read_bytes())  # old guard fails
        crlf = self.verify()
        self.assertEqual(crlf['observer_sha256'], crlf['git_blob_sha256'])
        self.assertEqual(lf['git_blob_sha256'], crlf['git_blob_sha256'])
        self.assertNotEqual(crlf['worktree_sha256'], crlf['git_blob_sha256'])

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
