import argparse
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import Mock, patch

import community_shard_six_machine as qual
import experiment_profile as profiles
import scaling_benchmark as benchmark
import scaling_phase as phase
import scaling_run as run
from test_community_shard_six_machine import make_identities


def synthetic_arms():
    workers = [f'{index:064x}' for index in range(12)]
    arms = []
    for name, count, offset in zip(profiles.OVERLAP.phases, (6, 12, 6), (10, 30, 60)):
        requests = []
        for index in range(count):
            start = (offset + index // 2) * 1_000_000_000
            requests.append({'id': f'{name}-{index + 1}', 'start_ns': start, 'end_ns': start + 1_000_000_000,
                             'success': True, 'output_hash': 'a' * 64,
                             'worker_ids': workers[:6] if index % 2 == 0 else workers[6:],
                             'server_verification_validated': True})
        arms.append({'workers': list(workers), 'requests': requests})
    return arms


class OverlapProfileTests(unittest.TestCase):
    def test_fixed_cohort_and_variable_request_counts_do_not_inflate_throughput(self):
        measured = benchmark.analyze_overlap(*synthetic_arms())
        self.assertEqual([measured['arms'][name]['completed_requests'] for name in profiles.OVERLAP.phases], [6, 12, 6])
        self.assertEqual([measured['arms'][name]['throughput_rps'] for name in profiles.OVERLAP.phases], [2.0, 2.0, 2.0])
        self.assertEqual(measured['comparison']['throughput_ratio'], 1.0)
        self.assertEqual(measured['arms']['overlap']['observed_client_in_flight_peak'], 2)
        self.assertTrue(measured['client_intervals_are_not_server_admission'])
        self.assertTrue(measured['no_real_operator_count'])
        self.assertNotIn('observed_max_concurrency', measured['arms']['overlap'])

    def test_mixed_shapes_coverage_hashes_and_excessive_concurrency_are_refused(self):
        for defect in ('six_middle', 'missing_worker', 'changed_cohort', 'hash_drift', 'five_in_flight', 'reused_id'):
            arms = synthetic_arms()
            if defect == 'six_middle':
                arms[1]['requests'] = arms[1]['requests'][:6]
            elif defect == 'missing_worker':
                for request in arms[0]['requests']:
                    request['worker_ids'] = arms[0]['workers'][:6]
            elif defect == 'changed_cohort':
                old, new = arms[2]['workers'][0], 'f' * 64
                arms[2]['workers'][0] = new
                for request in arms[2]['requests']:
                    request['worker_ids'] = [new if item == old else item for item in request['worker_ids']]
            elif defect == 'hash_drift':
                arms[1]['requests'][0]['output_hash'] = 'b' * 64
            elif defect == 'five_in_flight':
                for request in arms[1]['requests'][:5]:
                    request['start_ns'], request['end_ns'] = 30_000_000_000, 33_000_000_000
            else:
                arms[1]['requests'][0]['id'] = arms[0]['requests'][0]['id']
            with self.subTest(defect=defect), self.assertRaises(ValueError):
                benchmark.analyze_overlap(*arms)
        with self.assertRaises(ValueError):
            benchmark.analyze_scaling(*synthetic_arms())

    def test_mode_is_bound_to_cohort_markers_and_phase_order(self):
        identities = make_identities(slots=qual.SCALING_SLOTS)
        cohort = {'schema': profiles.OVERLAP.schema('cohort'), 'workflow_run_id': '9001', 'workflow_run_attempt': '1',
                  'candidate_run_id': '8001', 'candidate_run_attempt': '1', 'candidate_sha': '7145f682' + 'a' * 32,
                  'identities': identities, 'counts_toward_operator_target': False}
        with tempfile.TemporaryDirectory() as temp:
            directory = Path(temp)
            digest = run.write_new(directory / 'cohort.json', cohort)
            run.write_new(directory / 'cohort-digest.json', {'cohort_sha256': digest})
            self.assertEqual(run.read_cohort(directory, profiles.OVERLAP)[0], cohort)
            with self.assertRaises(qual.QualificationError):
                run.read_cohort(directory)
            run.write_new(directory / 'baseline_before-attempts.json', {'arm_status': 'failed'})
            with self.assertRaisesRegex(qual.QualificationError, 'previous arm did not succeed'):
                run.validate_phase_order(directory, 'overlap', profiles.OVERLAP)
            with self.assertRaisesRegex(qual.QualificationError, 'do not repeat inference POSTs'):
                run.validate_phase_order(directory, 'baseline_before', profiles.OVERLAP)
        marker = phase.marker_value('9001', '1', 'cohort', digest)
        with self.assertRaises(qual.QualificationError):
            phase.validate_marker(marker, '9001', '1', 'cohort', digest, profiles.OVERLAP)
        with self.assertRaises(qual.QualificationError):
            phase.artifact_name('9001', '1', 'expand', profiles.OVERLAP)

    def test_added_slots_start_with_cohort_and_stop_only_on_done(self):
        identities = {item['slot']: item for item in make_identities(slots=qual.SCALING_SLOTS)}
        digest = 'a' * 64
        for slot in (7, 12):
            with self.subTest(slot=slot), tempfile.TemporaryDirectory() as temp:
                root = Path(temp)
                identity = root / 'identity'
                identity.mkdir()
                (identity / 'worker-identity.json').write_text(json.dumps(identities[slot]))
                key = root / 'private-worker-key/key.json'
                key.parent.mkdir()
                key.write_text('{}')
                evidence = root / 'evidence'
                evidence.mkdir()
                (evidence / 'worker-evidence.json').write_text(json.dumps({'stop_reason': 'signal_15'}))
                args = argparse.Namespace(mode='overlap', slot=slot, workflow_run_id='9001', workflow_run_attempt='1',
                                          evidence_dir=str(evidence), identity_dir=str(identity), work_dir=str(root / 'work'),
                                          private_key_path=str(key), candidate_dir='candidate', model_path='model')
                markers = [{kind: phase.marker_value('9001', '1', kind, digest, profiles.OVERLAP)} for kind in ('cohort', 'done')]
                child = Mock()
                child.poll.return_value = None
                cohort = {'workflow_run_id': '9001', 'workflow_run_attempt': '1'}
                with patch.object(phase.PhaseReader, 'poll', side_effect=markers), patch.object(run, 'read_cohort', return_value=(cohort, identities, digest)), patch.object(phase.subprocess, 'Popen', return_value=child) as spawn, patch.object(phase, 'stop_child', return_value=1) as stop, patch.object(phase, 'validate_clean_worker'):
                    phase.supervise(args)
                spawn.assert_called_once()
                stop.assert_called_once()
                receipt = qual.read_bounded_json_file(evidence / 'overlap-supervisor.json')
                self.assertTrue(receipt['passed'])
                self.assertEqual(receipt['stop_kind'], 'done')
                self.assertFalse(key.exists())


if __name__ == '__main__':
    unittest.main()
