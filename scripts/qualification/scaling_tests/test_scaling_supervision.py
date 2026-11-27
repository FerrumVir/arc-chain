import argparse
import copy
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import Mock, patch

import community_shard_six_machine as qual
import scaling_phase as phase
from test_community_shard_six_machine import make_identities


class SupervisionTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.identities = {row['slot']: row for row in make_identities(slots=qual.SCALING_SLOTS)}
        self.digest = 'a' * 64
        self.cohort = {'workflow_run_id': '9001', 'workflow_run_attempt': '1'}

    def arguments(self, slot):
        folder = self.root / str(slot)
        folder.mkdir()
        identities = folder / 'identity'
        identities.mkdir()
        (identities / 'worker-identity.json').write_text(json.dumps(self.identities[slot]))
        key = folder / 'private-worker-key/key.json'
        key.parent.mkdir()
        key.write_text('{}')
        evidence = folder / 'evidence'
        evidence.mkdir()
        (evidence / 'worker-evidence.json').write_text(json.dumps({'stop_reason': 'signal_15'}))
        return argparse.Namespace(slot=slot, workflow_run_id='9001', workflow_run_attempt='1', evidence_dir=str(evidence), identity_dir=str(identities), work_dir=str(folder / 'work'), private_key_path=str(key), candidate_dir='candidate', model_path='model')

    def marker(self, kind):
        return phase.marker_value('9001', '1', kind, self.digest)

    def receipt(self, args):
        return qual.read_bounded_json_file(Path(args.evidence_dir) / 'scaling-supervisor.json')

    def test_historical_real_cleanup_passes_and_missing_proof_fields_fail(self):
        fixture = json.loads((Path(__file__).parent / 'fixtures/six_machine_worker_cleanup.json').read_text())
        summary = phase.validate_clean_worker(fixture['evidence'], fixture['identity'])
        self.assertGreater(summary['steps_executed'], 0)
        for key, value in [('key_deleted', False), ('systemd_memory_peak_bytes', None), ('exit_code', True), ('parse_error', 'lost stdout')]:
            changed = dict(fixture['evidence'], **{key: value})
            with self.subTest(key=key), self.assertRaises(qual.QualificationError):
                phase.validate_clean_worker(changed, fixture['identity'])
        identity = copy.deepcopy(fixture['identity'])
        evidence = copy.deepcopy(fixture['evidence'])
        del identity['boot_id']
        del evidence['boot_id']
        with self.assertRaisesRegex(qual.QualificationError, 'boot_id'):
            phase.validate_clean_worker(evidence, identity)

    def test_unknown_error_never_writes_success_and_key_is_cleaned(self):
        args = self.arguments(1)
        with patch.object(phase.PhaseReader, 'poll', side_effect=RuntimeError('unexpected failure')):
            with self.assertRaises(RuntimeError):
                phase.supervise(args)
        self.assertFalse(self.receipt(args)['passed'])
        self.assertTrue(self.receipt(args)['key_deleted'])
        self.assertIsNotNone(self.receipt(args)['error'])

    def test_malformed_identity_still_deletes_owned_key(self):
        args = self.arguments(1)
        Path(args.identity_dir, 'worker-identity.json').write_text('[]')
        with self.assertRaises(qual.QualificationError):
            phase.supervise(args)
        self.assertFalse(Path(args.private_key_path).exists())
        self.assertFalse(self.receipt(args)['passed'])

    def test_added_cohort_does_not_start_after_stop_added(self):
        args = self.arguments(7)
        with patch.object(phase.PhaseReader, 'poll', return_value={kind: self.marker(kind) for kind in ('cohort', 'expand', 'stop-added')}), patch.object(phase.subprocess, 'Popen') as spawn:
            with self.assertRaisesRegex(qual.QualificationError, 'stopped before'):
                phase.supervise(args)
        spawn.assert_not_called()
        self.assertEqual(self.receipt(args)['stop_kind'], 'stop-added-before-start')

    def test_stop_added_takes_priority_over_later_global_done(self):
        args = self.arguments(7)
        child = Mock()
        child.poll.return_value = None
        phases = [{'cohort': self.marker('cohort'), 'expand': self.marker('expand')}, {'done': self.marker('done'), 'stop-added': self.marker('stop-added')}]
        with patch.object(phase.PhaseReader, 'poll', side_effect=phases), patch.object(phase.run, 'read_cohort', return_value=(self.cohort, self.identities, self.digest)), patch.object(phase.subprocess, 'Popen', return_value=child), patch.object(phase, 'stop_child', return_value=1) as stop, patch.object(phase, 'validate_clean_worker'):
            phase.supervise(args)
        stop.assert_called_once()
        self.assertTrue(self.receipt(args)['passed'])
        self.assertEqual(self.receipt(args)['stop_kind'], 'stop-added')

    def test_timeout_does_not_repeat_cleanup_wait(self):
        args = self.arguments(1)
        child = Mock()
        child.poll.return_value = None
        with patch.object(phase.PhaseReader, 'poll', side_effect=[{'cohort': self.marker('cohort')}, {'done': self.marker('done')}]), patch.object(phase.run, 'read_cohort', return_value=(self.cohort, self.identities, self.digest)), patch.object(phase.subprocess, 'Popen', return_value=child), patch.object(phase, 'stop_child', side_effect=subprocess.TimeoutExpired('owned child', 120)) as stop:
            with self.assertRaises(qual.QualificationError):
                phase.supervise(args)
        stop.assert_called_once()
        self.assertFalse(self.receipt(args)['passed'])
        self.assertTrue(self.receipt(args)['key_deleted'])

    def test_other_workflow_cohort_cannot_start_worker(self):
        args = self.arguments(1)
        with patch.object(phase.PhaseReader, 'poll', return_value={'cohort': self.marker('cohort')}), patch.object(phase.run, 'read_cohort', return_value=({'workflow_run_id': '9002', 'workflow_run_attempt': '1'}, self.identities, self.digest)), patch.object(phase.subprocess, 'Popen') as spawn:
            with self.assertRaisesRegex(qual.QualificationError, 'another workflow'):
                phase.supervise(args)
        spawn.assert_not_called()


if __name__ == '__main__':
    unittest.main()
