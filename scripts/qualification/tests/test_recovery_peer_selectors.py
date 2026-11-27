import importlib.util
from pathlib import Path
import unittest


SOURCE = Path(__file__).resolve().parents[1] / "run-recovery-peer-tests.py"
SPEC = importlib.util.spec_from_file_location("recovery_peer_tests", SOURCE)
RUNNER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(RUNNER)


class RecoveryPeerSelectorTests(unittest.TestCase):
    def test_all_mandatory_cases_are_selected_before_compilation(self):
        targets = RUNNER.expected_test_targets()
        prefixes, expected = targets["arc_state"]
        regression = "wal::tests::verified_recovery_identity_probe_preserves_writer_and_rejects_path_replacement"
        self.assertIn(regression, expected)
        self.assertTrue(regression.startswith(prefixes))

    def test_missing_prefix_names_the_required_fix(self):
        with self.assertRaisesRegex(ValueError, "add their prefixes before compiling"):
            RUNNER.validate_selector_prefixes({"state": (("old::",), {"new::required_test"})})


if __name__ == "__main__":
    unittest.main()
