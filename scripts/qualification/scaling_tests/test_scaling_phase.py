import argparse
import json
from pathlib import Path
import signal
import tempfile
import unittest
from unittest.mock import Mock, patch

import community_shard_six_machine as qual
import scaling_phase as phase
import scaling_run as run
from test_community_shard_six_machine import make_identities


class PhaseTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.directory = Path(temporary.name)

    def test_marker_rejects_wrong_run_attempt_phase_and_cohort(self):
        digest = 'a' * 64
        value = phase.marker_value('9001', '1', 'expand', digest)
        self.assertEqual(value, phase.validate_marker(value, '9001', '1', 'expand', digest))
        for field, replacement in [('workflow_run_id', '9002'), ('workflow_run_attempt', '2'), ('kind', 'stop-added'), ('cohort_sha256', 'b' * 64), ('counts_toward_operator_target', True)]:
            changed = dict(value, **{field: replacement})
            with self.subTest(field=field), self.assertRaises(qual.QualificationError):
                phase.validate_marker(changed, '9001', '1', 'expand', digest)
        with self.assertRaises(qual.QualificationError):
            phase.marker_value('9001', '1', 'expand', None)
        self.assertIsNone(phase.marker_value('9001', '1', 'done', None)['cohort_sha256'])

    def test_waiting_time_is_subtracted_from_worker_runtime(self):
        self.assertEqual(phase.remaining_runtime(3600, 900), 2580)
        self.assertEqual(phase.remaining_runtime(3600, 0), 3480)
        with self.assertRaises(qual.QualificationError):
            phase.remaining_runtime(3600, 3421)

    def test_only_owned_child_receives_sigterm_and_has_120_seconds(self):
        child = Mock()
        child.poll.return_value = None
        child.wait.return_value = 1
        self.assertEqual(phase.stop_child(child), 1)
        child.send_signal.assert_called_once_with(signal.SIGTERM)
        child.wait.assert_called_once_with(timeout=120)
        child.kill.assert_not_called()

    def test_artifact_download_uses_verified_digest_helper_and_bounded_time(self):
        marker = phase.marker_value('9001', '1', 'expand', 'a' * 64)
        metadata = {'id': 32, 'name': phase.artifact_name('9001', '1', 'expand'), 'expired': False, 'digest': 'sha256:' + 'b' * 64}
        reader = phase.PhaseReader('9001', '1', self.directory, 'test-token', 3600)
        def download(run_id, name, row, target, maximum, token, **kwargs):
            self.assertEqual(row, metadata)
            self.assertEqual(kwargs['timeout_seconds'], 50)
            target.mkdir()
            (target / 'phase.json').write_text(json.dumps(marker))
        with patch.object(qual, 'github_api_get_json', return_value={'artifacts': [metadata]}), patch.object(qual, 'download_validated_artifact_by_id', side_effect=download) as fetch, patch.object(phase.time, 'monotonic', return_value=3430):
            self.assertEqual(reader.poll(('expand',)), {'expand': marker})
            self.assertEqual(reader.poll(('expand',)), {'expand': marker})
            self.assertEqual(fetch.call_count, 1)
        reader.deadline = phase.time.monotonic() + 3600
        with patch.object(qual, 'github_api_get_json', return_value={'artifacts': [metadata, dict(metadata, id=33)]}):
            with self.assertRaisesRegex(qual.QualificationError, 'duplicate phase artifact'):
                reader.poll(('expand',))
        with patch.object(qual, 'github_api_get_json', return_value={'artifacts': [dict(metadata, id=33)]}):
            with self.assertRaisesRegex(qual.QualificationError, 'changed after verification'):
                reader.poll(('expand',))

    def test_global_done_prevents_spawn_and_deletes_owned_private_key(self):
        identity = make_identities()[0]
        identity_dir = self.directory / 'identity'
        identity_dir.mkdir()
        (identity_dir / 'worker-identity.json').write_text(json.dumps(identity))
        key = self.directory / 'private-worker-key' / 'key.json'
        key.parent.mkdir()
        key.write_text('{}')
        args = argparse.Namespace(slot=1, workflow_run_id='9001', workflow_run_attempt='1', evidence_dir=str(self.directory / 'evidence'), identity_dir=str(identity_dir), work_dir=str(self.directory / 'work'), private_key_path=str(key), candidate_dir='candidate', model_path='model')
        with patch.object(phase.PhaseReader, 'poll', return_value={'done': phase.marker_value('9001', '1', 'done', None)}), patch.object(phase.subprocess, 'Popen') as spawn:
            with self.assertRaisesRegex(qual.QualificationError, 'finished before'):
                phase.supervise(args)
        spawn.assert_not_called()
        self.assertFalse(key.exists())
        receipt = qual.read_bounded_json_file(Path(args.evidence_dir) / 'scaling-supervisor.json')
        self.assertEqual(receipt['stop_kind'], 'done-before-start')
        self.assertFalse(receipt['passed'])
        self.assertTrue(receipt['key_deleted'])


if __name__ == '__main__':
    unittest.main()
