"""Exercise the actual harness assignment under errexit; never run the harness/node."""
import json
from pathlib import Path
import subprocess
import tempfile
import unittest

HERE = Path(__file__).resolve().parent
ASSIGNMENTS = [line for line in (HERE / 'headless-v07-acceptance.sh').read_text().splitlines()
               if line.startswith('privacy_safe=')]
assert len(ASSIGNMENTS) == 1, 'expected exactly one privacy flag assignment'
SCRIPT = 'set -euo pipefail\npins=$1\n' + ASSIGNMENTS[0] + '\nprintf "parsed=%s\\n" "$privacy_safe"\n'


class PrivacyFlag(unittest.TestCase):
    def parse(self, raw):
        with tempfile.TemporaryDirectory(dir=HERE) as directory:
            path = Path(directory) / 'pins.json'
            path.write_text(raw)
            return subprocess.run(['bash', '-c', SCRIPT, 'privacy-test', str(path)],
                                  capture_output=True, text=True)

    def test_true_and_false_survive_errexit(self):
        for value in (True, False):
            with self.subTest(value=value):
                result = self.parse(json.dumps({'node_release': {'worker_names_privacy_safe': value}}))
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout, f'parsed={str(value).lower()}\n')

    def test_missing_and_nonboolean_fail_before_continuation(self):
        docs = [{}, {'node_release': {}}, {'node_release': None}]
        docs += [{'node_release': {'worker_names_privacy_safe': value}}
                 for value in (None, 'true', 'false', '', 0, 1, [], {})]
        for doc in docs:
            with self.subTest(doc=doc):
                result = self.parse(json.dumps(doc))
                self.assertNotEqual(result.returncode, 0)
                self.assertNotIn('parsed=', result.stdout)
                self.assertIn('privacy flag must be boolean', result.stderr)

    def test_malformed_json_fails_before_continuation(self):
        result = self.parse('{')
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn('parsed=', result.stdout)


if __name__ == '__main__':
    unittest.main()
