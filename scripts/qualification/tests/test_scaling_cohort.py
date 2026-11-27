import copy
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import community_shard_six_machine as qual
from test_community_shard_six_machine import make_identities


class ScalingCohortTests(unittest.TestCase):
    def validate(self, identities, **kwargs):
        return qual.validate_worker_identities(identities, '9001', '1', '8001', '1', '7145f682' + 'a' * 32, **kwargs)

    def test_six_worker_default_cannot_silently_become_twelve(self):
        twelve = make_identities(slots=qual.SCALING_SLOTS)
        with self.assertRaisesRegex(qual.QualificationError, 'expected 6 worker identities'):
            self.validate(twelve)
        self.assertEqual(len(self.validate(twelve, expected_slots=qual.SCALING_SLOTS)), 12)
        self.assertEqual(len(self.validate(make_identities())), 6)

    def test_extra_workers_require_independent_machine_and_identity(self):
        identities = make_identities(slots=qual.SCALING_SLOTS)
        for key in ('boot_id', 'runner_name', 'worker_id'):
            with self.subTest(key=key):
                duplicate = copy.deepcopy(identities)
                duplicate[6][key] = duplicate[0][key]
                with self.assertRaisesRegex(qual.QualificationError, 'duplicate'):
                    self.validate(duplicate, expected_slots=qual.SCALING_SLOTS)

    def test_wrong_cohort_or_unbounded_slots_fail(self):
        for slots in ((1, 2, 3), tuple(range(1, 14)), list(range(1, 13))):
            with self.assertRaisesRegex(qual.QualificationError, 'select exactly'):
                self.validate([], expected_slots=slots)
        for slot in (0, 13, True, 1.5):
            with self.assertRaises(qual.QualificationError):
                qual.validate_slot(slot)
        self.assertEqual(qual.DEFAULT_WORKER_SUPERVISOR_MAX_SECONDS, 2400)


if __name__ == '__main__':
    unittest.main()
