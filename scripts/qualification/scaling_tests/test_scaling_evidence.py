import copy
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import community_shard_six_machine as qual
import scaling_aggregate as aggregate
import scaling_phase as phase
import scaling_run as run
from test_scaling_controller import BOOT_NONCE, make_dispatch_response, make_workers


class EvidenceTests(unittest.TestCase):
    def report(self, name, workers):
        attempts = []
        for index in range(1, 7):
            response = make_dispatch_response(workers[:6], seq=index)
            attempts.append({'id': name + '-' + str(index), 'status': 'succeeded', 'http_status': 200,
                             'dispatch_uncertain': False, 'request_payload': dict(qual.DISPATCH_PAYLOAD),
                             'start_ns': index * 100, 'end_ns': index * 100 + 50,
                             'parsed_response': response})
        return {'arm_status': 'succeeded', 'attempts': attempts}

    def test_reconstructs_from_raw_requests_and_rejects_uncertain_or_foreign_work(self):
        workers = make_workers(6)
        report = self.report('before', workers)
        arm = run.reconstruct_arm('before', report, set(workers), BOOT_NONCE)
        self.assertEqual(len(arm['requests']), 6)
        self.assertEqual(arm['requests'][0]['start_ns'], 100)
        for change in ({'dispatch_uncertain': True}, {'http_status': True}, {'request_payload': {}}, {'id': 'before-2'}, {'status': 'failed'}):
            modified = copy.deepcopy(report)
            modified['attempts'][0].update(change)
            with self.subTest(change=change), self.assertRaises(qual.QualificationError):
                run.reconstruct_arm('before', modified, set(workers), BOOT_NONCE)
        modified = copy.deepcopy(report)
        modified['attempts'][0]['parsed_response']['participants'][0]['worker_id'] = 'b' * 64
        with self.assertRaisesRegex(qual.QualificationError, 'foreign'):
            run.reconstruct_arm('before', modified, set(workers), BOOT_NONCE)

    def test_real_api_object_listing_is_used_and_deadline_covers_every_page(self):
        rows = [{'name': 'artifact-' + str(index), 'expired': False, 'id': index + 1} for index in range(100)]
        with patch.object(qual, 'github_api_get_json', side_effect=[{'artifacts': rows}, {'artifacts': []}]) as fetch, patch.object(run.time, 'monotonic', side_effect=[100, 110]):
            result = run.list_artifacts('9001', 'test-token', 120)
        self.assertEqual(len(result), 100)
        self.assertEqual(fetch.call_args_list[0].kwargs['timeout_seconds'], 15)
        self.assertEqual(fetch.call_args_list[1].kwargs['timeout_seconds'], 10)
        with patch.object(qual, 'github_api_get_json', return_value={'artifacts': rows}) as fetch, patch.object(run.time, 'monotonic', side_effect=[100, 121]):
            with self.assertRaisesRegex(qual.QualificationError, 'deadline'):
                run.list_artifacts('9001', 'test-token', 120)
        self.assertEqual(fetch.call_count, 1)

    def test_pagination_deduplicates_same_artifact_id_but_preserves_distinct_artifacts(self):
        rows = [{'name': 'artifact-' + str(index), 'expired': False, 'id': index + 1} for index in range(100)]
        second = [rows[-1], dict(rows[-1], id=101)]
        with patch.object(qual, 'github_api_get_json', side_effect=[{'artifacts': rows}, {'artifacts': second}]):
            result = run.list_artifacts('9001', 'test-token', run.time.monotonic() + 120)
        self.assertEqual(len(result), 101)
        self.assertEqual(sum(row['name'] == 'artifact-99' for row in result), 2)


    def test_vanished_verified_marker_fails_instead_of_forgetting_phase(self):
        with tempfile.TemporaryDirectory() as temp:
            reader = phase.PhaseReader('9001', '1', Path(temp), 'test-token', phase.time.monotonic() + 3600)
            reader.cache['expand'] = ((1, 'sha256:' + 'a' * 64), {})
            with patch.object(qual, 'github_api_get_json', return_value={'artifacts': []}):
                with self.assertRaisesRegex(qual.QualificationError, 'disappeared'):
                    reader.poll(('expand',))

    def test_artifact_helper_receives_exact_metadata_from_api(self):
        metadata = {'name': 'evidence', 'expired': False, 'id': 12, 'digest': 'sha256:' + 'a' * 64}
        with patch.object(qual, 'download_validated_artifact_by_id') as download, patch.object(qual, 'verify_evidence_dir_no_secrets') as verify:
            value = aggregate.download_one('9001', 'evidence', [metadata], Path('unused'), 'test-token')
        self.assertEqual(value['digest'], metadata['digest'])
        self.assertEqual(download.call_args.args[2], metadata)
        verify.assert_called_once_with(Path('unused'))
        with self.assertRaises(qual.QualificationError):
            aggregate.download_one('9001', 'evidence', [metadata, metadata], Path('unused'), 'test-token')

    def test_twelve_worker_aggregate_keeps_per_artifact_bounds_and_publishes_compact_summary(self):
        def transfer(workflow, name, metadata, destination, maximum, token, **kwargs):
            destination.mkdir()
            for index in range(6):
                (destination / f'evidence-{index}.json').write_text('{}')

        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            output, artifacts = aggregate.prepare_aggregation_paths(root / 'summary', root / 'artifacts')
            with patch.object(qual, 'download_validated_artifact_by_id', side_effect=transfer):
                for index in range(13):
                    name = f'artifact-{index}'
                    metadata = {'name': name, 'expired': False, 'id': index + 1, 'digest': 'sha256:' + 'a' * 64}
                    aggregate.download_one('9001', name, [metadata], artifacts / name, 'test-token')
            with self.assertRaisesRegex(qual.QualificationError, 'file count exceeded 64'):
                qual.inspect_tree_bounds(artifacts, qual.MAX_EVIDENCE_DIR_BYTES)
            run.write_new(output / 'qualification-summary.json', {'overall_status': 'passed'})
            qual.verify_evidence_dir_no_secrets(output)
            self.assertTrue((output / 'qualification-summary.json').is_file())
            self.assertEqual(len(list(artifacts.rglob('*.json'))), 78)

    def test_aggregate_rejects_overlapping_directories_and_secret_bearing_downloads(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            for output, artifacts in [(root, root), (root / 'summary', root), (root, root / 'artifacts')]:
                with self.subTest(output=output, artifacts=artifacts), self.assertRaisesRegex(qual.QualificationError, 'separate directories'):
                    aggregate.prepare_aggregation_paths(output, artifacts)
            metadata = {'name': 'evidence', 'expired': False, 'id': 1, 'digest': 'sha256:' + 'a' * 64}
            target = root / 'download'

            def transfer(*args, **kwargs):
                target.mkdir()
                (target / 'worker-key.json').write_text('{}')

            with patch.object(qual, 'download_validated_artifact_by_id', side_effect=transfer):
                with self.assertRaisesRegex(qual.QualificationError, 'forbidden file'):
                    aggregate.download_one('9001', 'evidence', [metadata], target, '')
            self.assertFalse(list(target.glob('*')))


if __name__ == '__main__':
    unittest.main()
