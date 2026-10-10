"""Cross-host gates and honest resource reporting, without model dependencies."""
import copy
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

from arc_quality.layer_probe.compare import canonical
from arc_quality.layer_probe.cross_host import compare_hosts
from arc_quality.layer_probe.matrix import process_cell
from arc_quality.layer_probe import measured
from arc_quality.layer_probe.policies import RUN_NAMES


class HostEvidenceTests(unittest.TestCase):
    def test_cross_host_requires_all_records_and_exact_arc(self):
        with tempfile.TemporaryDirectory() as tmp:
            left, right = Path(tmp)/'left', Path(tmp)/'right'
            arc = {'alignment': {'pin': 'same'}, 'tensor_sha256': {'logits': 'integer'},
                   'tensors': {'logits': [[1, 2]]}, 'routing': {'experts': [0, 1]}}
            ref = {'alignment': arc['alignment'], 'provenance': {'weight_tensors': {'w': 'same'}},
                   'tensor_sha256': {'logits': 'float'}, 'tensors': {'logits': [[1.0, 2.0]]}}
            for root in (left, right):
                for depth in (1, 2, 3):
                    for name in RUN_NAMES:
                        path = root/f'depth-{depth}'/name
                        path.mkdir(parents=True)
                        (path/'arc.json').write_bytes(canonical(arc))
                        (path/'reference.json').write_bytes(canonical(ref))
            self.assertEqual(len(compare_hosts(left, right)['runs']), 24)
            path = right/'depth-3/head'
            changed = copy.deepcopy(ref)
            changed['tensors']['logits'][0][0] += 1e-6
            changed['tensor_sha256']['logits'] = 'host-specific-float'
            (path/'reference.json').write_bytes(canonical(changed))
            self.assertFalse(compare_hosts(left, right)['runs']['depth-3/head']['reference_tensors_equal'])
            changed['provenance']['weight_tensors']['w'] = 'different'
            (path/'reference.json').write_bytes(canonical(changed))
            with self.assertRaises(AssertionError):
                compare_hosts(left, right)
            (path/'reference.json').write_bytes(canonical(ref))
            for field in ('alignment', 'tensors', 'routing'):
                changed = copy.deepcopy(arc)
                changed[field] = {'different': True}
                (path/'arc.json').write_bytes(canonical(changed))
                with self.subTest(field=field), self.assertRaises(AssertionError):
                    compare_hosts(left, right)
            (path/'arc.json').unlink()
            with self.assertRaises(FileNotFoundError):
                compare_hosts(left, right)

    def test_windows_measurement_is_explicitly_unavailable_and_preserves_exit(self):
        with tempfile.TemporaryDirectory() as tmp:
            output = Path(tmp)/'resources.json'
            argv = ['measured', str(output), sys.executable, '-c', 'raise SystemExit(7)']
            with patch.object(sys, 'argv', argv), patch.object(sys, 'platform', 'win32'):
                with self.assertRaises(SystemExit) as result:
                    measured.main()
            self.assertEqual(result.exception.code, 7)
            data = json.loads(output.read_text())
            self.assertEqual(data['exit_code'], 7)
            self.assertGreater(data['wall_seconds'], 0)
            for key in ('peak_rss_bytes', 'user_seconds', 'system_seconds'):
                self.assertIsNone(data[key])
            self.assertIn('not measured', process_cell(data))


if __name__ == '__main__':
    unittest.main()
