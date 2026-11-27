import argparse
import contextlib
import copy
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import community_shard_six_machine as qual
import scaling_run as run
from test_community_shard_six_machine import make_identities
from test_scaling_controller import BOOT_NONCE, make_status


class ScalingRunTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name)
        self.args = argparse.Namespace(output_dir=str(self.directory), workflow_run_id='9001', workflow_run_attempt='1', candidate_run_id='8001', candidate_run_attempt='1', candidate_sha='7145f682' + 'a' * 32)
        self.status = make_status(0, 0, 0, 0)
        self.identities = make_identities(slots=qual.SCALING_SLOTS)
        self.artifacts = [{'name': qual.make_identity_artifact_name('9001', '1', s), 'id': s, 'expired': False, 'digest': 'sha256:' + 'a' * 64} for s in qual.SCALING_SLOTS]

    def transfer(self, workflow, name, metadata, directory, bound, token, **kwargs):
        self.assertEqual(workflow, '9001')
        self.assertTrue(0 < kwargs['timeout_seconds'] <= 120)
        directory.mkdir()
        (directory / 'worker-identity.json').write_text(json.dumps(self.identities[metadata['id'] - 1]))

    def initialize(self):
        with patch.object(qual, 'validate_candidate_run_api'), patch.object(qual, 'github_api_get_json', return_value={'artifacts': self.artifacts}), patch.object(qual, 'download_validated_artifact_by_id', side_effect=self.transfer), patch.object(qual, 'coordinator_get_json_with_retries', return_value=self.status), contextlib.redirect_stdout(io.StringIO()):
            run.initialize(self.args)

    def test_twelve_distinct_identities_and_cohort_digest_are_bound(self):
        self.initialize()
        cohort, identities, digest = run.read_cohort(self.directory)
        self.assertEqual(len(identities), 12)
        self.assertEqual(cohort['coordinator_boot_nonce'], BOOT_NONCE)
        self.assertFalse(cohort['counts_toward_operator_target'])
        self.assertEqual(len(digest), 64)
        path = self.directory / 'cohort.json'
        path.write_bytes(path.read_bytes() + b' ')
        with self.assertRaisesRegex(qual.QualificationError, 'cohort bytes changed'):
            run.read_cohort(self.directory)

    def test_duplicate_artifact_or_machine_never_publishes_cohort(self):
        self.artifacts.append(dict(self.artifacts[0], id=99))
        with self.assertRaisesRegex(qual.QualificationError, 'duplicate identity artifact'):
            self.initialize()
        self.assertFalse((self.directory / 'cohort.json').exists())
        self.artifacts.pop()
        self.identities[6]['boot_id'] = self.identities[0]['boot_id']
        with self.assertRaisesRegex(qual.QualificationError, 'duplicate machine boot_id'):
            self.initialize()
        self.assertFalse((self.directory / 'cohort.json').exists())

    def test_identity_timeout_does_not_start_workers_or_download(self):
        with patch.object(qual, 'validate_candidate_run_api'), patch.object(qual, 'github_api_get_json', return_value={'artifacts': []}), patch.object(run.time, 'monotonic', side_effect=[0, 901]), patch.object(qual, 'download_validated_artifact_by_id') as download:
            with self.assertRaisesRegex(qual.QualificationError, 'artifact listing exceeded'):
                run.initialize(self.args)
        download.assert_not_called()

    def test_failed_or_missing_prior_arm_blocks_next_measurement(self):
        with self.assertRaisesRegex(qual.QualificationError, 'previous arm evidence missing'):
            run.validate_phase_order(self.directory, 'expanded')
        run.write_new(self.directory / 'before-attempts.json', {'arm_status': 'failed'})
        with self.assertRaisesRegex(qual.QualificationError, 'previous arm did not succeed'):
            run.validate_phase_order(self.directory, 'expanded')

    def test_preexisting_workers_block_cohort_publication(self):
        self.status = make_status(6, 0, 0, 1)
        with self.assertRaises(qual.QualificationError):
            self.initialize()
        self.assertFalse((self.directory / 'cohort.json').exists())

    def test_uncertain_attempt_blocks_repeated_arm_before_any_post(self):
        run.write_new(self.directory / 'before-attempts.json', {'arm_status': 'failed', 'attempts': [{'dispatch_uncertain': True}]})
        with self.assertRaisesRegex(qual.QualificationError, 'do not repeat inference POSTs'):
            run.validate_phase_order(self.directory, 'before')


    def test_create_only_writer_rejects_replacement(self):
        path = self.directory / 'evidence.json'
        run.write_new(path, {'first': True})
        with self.assertRaises(FileExistsError):
            run.write_new(path, {'first': False})
        self.assertEqual(json.loads(path.read_text()), {'first': True})


if __name__ == '__main__':
    unittest.main()
