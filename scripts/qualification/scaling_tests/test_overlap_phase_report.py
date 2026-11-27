import hashlib
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import community_shard_six_machine as qual
import experiment_profile as profiles
import overlap_phase_report as report
import scaling_aggregate as aggregate
import scaling_benchmark as benchmark
import scaling_run as run
import shard_phase_evidence as phases
from test_community_shard_six_machine import make_identities
from test_scaling_controller import BOOT_NONCE, make_dispatch_response


def write_synthetic_controller(directory):
    identities = make_identities(slots=qual.SCALING_SLOTS)
    workers = [item['worker_id'] for item in identities]
    cohort = {'schema': profiles.OVERLAP.schema('cohort'), 'workflow_run_id': '9001', 'workflow_run_attempt': '1',
              'candidate_run_id': '8001', 'candidate_run_attempt': '1', 'candidate_sha': '7145f682' + 'a' * 32,
              'identities': identities, 'counts_toward_operator_target': False, 'coordinator_boot_nonce': BOOT_NONCE}
    digest = run.write_new(directory / 'cohort.json', cohort)
    run.write_new(directory / 'cohort-digest.json', {'cohort_sha256': digest})
    arms, sequence = [], 0
    for name, count, cap, offset in zip(profiles.OVERLAP.phases, (6, 12, 6), (2, 4, 2), (10, 30, 60)):
        attempts = []
        for index in range(count):
            sequence += 1
            start = (offset + index // 2) * 1_000_000_000
            attempts.append({'id': f'{name}-{index + 1}', 'status': 'succeeded', 'http_status': 200,
                             'dispatch_uncertain': False, 'request_payload': dict(qual.DISPATCH_PAYLOAD),
                             'start_ns': start, 'end_ns': start + 1_000_000_000,
                             'parsed_response': make_dispatch_response(workers[:6] if index % 2 == 0 else workers[6:], seq=sequence)})
        evidence = {'arm_status': 'succeeded', 'experiment_mode': 'overlap', 'max_concurrency': cap, 'attempts': attempts}
        run.write_new(directory / (name + '-attempts.json'), evidence)
        arm = run.reconstruct_arm(name, evidence, set(workers), BOOT_NONCE, profiles.OVERLAP)
        run.write_new(directory / (name + '-arm.json'), arm)
        arms.append(arm)
    run.write_new(directory / profiles.OVERLAP.measurement_file, benchmark.analyze_overlap(*arms))


def synthetic_journal(skip=None):
    lines = []
    for sequence in range(1, 25):
        for phase in phases.PHASES:
            if (sequence, phase) == skip:
                continue
            message = (f'2026-11-27T10:00:00Z  INFO {phases.PHASE_TARGET}: {phases.PHASE_MESSAGE} '
                       f'boot_nonce="{BOOT_NONCE}" session_seq={sequence} phase="{phase}" duration_ms=10 returned_ok=true')
            lines.append(json.dumps({'MESSAGE': message}))
    return '\n'.join(lines)


class OverlapPhaseReportTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.controller = self.root / 'controller'
        write_synthetic_controller(self.controller)
        self.journal = self.root / 'journal.jsonl'
        self.journal.write_text(synthetic_journal())

    def test_real_validation_path_correlates_twenty_four_synthetic_sessions(self):
        value = report.build_report(self.controller, self.journal)
        self.assertTrue(value['phase_records_complete'])
        self.assertEqual(value['phase_evidence']['complete_ok_session_count'], 24)
        self.assertEqual(len(value['controller_files_sha256']), 9)
        self.assertFalse(value['counts_toward_operator_target'])
        self.assertFalse(value['proves_server_admission_concurrency'])
        self.assertIn('not_independently_authenticated', value['journal_authentication'])
        self.assertNotIn('MESSAGE', json.dumps(value))

    def test_missing_phase_is_missing_and_unverified_response_never_counts(self):
        self.journal.write_text(synthetic_journal((1, 'quorum_verification')))
        value = report.build_report(self.controller, self.journal)
        self.assertFalse(value['phase_records_complete'])
        self.assertIsNone(value['phase_evidence']['sessions'][0]['phases']['quorum_verification'])
        path = self.controller / 'baseline_before-attempts.json'
        data = json.loads(path.read_text())
        data['attempts'][0]['dispatch_uncertain'] = True
        path.write_text(json.dumps(data))
        with self.assertRaisesRegex(qual.QualificationError, 'uncertain POST'):
            report.build_report(self.controller, self.journal)

    def test_source_changes_after_validation_cannot_change_captured_session_keys(self):
        path = self.controller / 'baseline_before-attempts.json'
        original_hash = hashlib.sha256(path.read_bytes()).hexdigest()
        real_verify = aggregate.verify_measurements
        def verify_and_mutate(snapshot, profile):
            result = real_verify(snapshot, profile)
            path.write_text('{}')
            return result
        with patch.object(aggregate, 'verify_measurements', side_effect=verify_and_mutate):
            value = report.build_report(self.controller, self.journal)
        self.assertTrue(value['phase_records_complete'])
        self.assertEqual(value['controller_files_sha256'][path.name], original_hash)

    def test_symlink_and_oversized_journal_refused(self):
        link = self.root / 'link.jsonl'
        link.symlink_to(self.journal)
        with self.assertRaises(OSError):
            report.build_report(self.controller, link)
        with patch.object(phases, 'MAX_JOURNAL_BYTES', 4):
            with self.assertRaisesRegex(qual.QualificationError, 'bounded regular file'):
                report.build_report(self.controller, self.journal)


if __name__ == '__main__':
    unittest.main()
