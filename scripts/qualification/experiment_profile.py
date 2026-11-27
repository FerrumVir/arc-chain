from dataclasses import dataclass

import community_shard_six_machine as qual


@dataclass(frozen=True)
class ExperimentProfile:
    name: str
    phases: tuple[str, str, str]
    worker_counts: tuple[int, int, int]
    request_counts: tuple[int, int, int]
    offered_caps: tuple[int, int, int]
    marker_kinds: tuple[str, ...]

    def phase_index(self, name):
        if name not in self.phases:
            raise qual.QualificationError('phase does not belong to ' + self.name + ': ' + str(name))
        return self.phases.index(name)

    def slots(self, phase):
        count = self.worker_counts[self.phase_index(phase)]
        return qual.SCALING_SLOTS if count == 12 else qual.EXPECTED_SLOTS

    def requests(self, phase):
        return self.request_counts[self.phase_index(phase)]

    def schema(self, kind):
        if kind not in ('cohort', 'phase', 'supervisor', 'qualification'):
            raise qual.QualificationError('unknown experiment evidence kind')
        return 'arc.community-shard-' + self.name + '.' + kind + '.v1'

    def stop_kind(self, slot):
        qual.validate_slot(slot)
        return 'stop-added' if self.name == 'scaling' and slot > 6 else 'done'

    @property
    def measurement_file(self):
        return self.name + '-measurement.json'


SCALING = ExperimentProfile('scaling', ('before', 'expanded', 'after'), (6, 12, 6),
                            (6, 6, 6), (1, 2, 1), ('cohort', 'expand', 'stop-added', 'done'))
OVERLAP = ExperimentProfile('overlap', ('baseline_before', 'overlap', 'baseline_after'), (12, 12, 12),
                            (6, 12, 6), (2, 4, 2), ('cohort', 'done'))


def select(mode):
    if mode == 'scaling':
        return SCALING
    if mode == 'overlap':
        return OVERLAP
    raise qual.QualificationError('experiment mode must be scaling or overlap')
