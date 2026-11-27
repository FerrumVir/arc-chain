import argparse
import hashlib
import json
import os
from pathlib import Path
import stat
import tempfile

import community_shard_six_machine as qual
import experiment_profile as experiments
import scaling_aggregate as aggregate
import scaling_run as run
import shard_phase_evidence as phases


def read_bounded_regular(path, maximum):
    descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    with os.fdopen(descriptor, 'rb') as stream:
        before = os.fstat(stream.fileno())
        run.need(stat.S_ISREG(before.st_mode) and before.st_size <= maximum,
                 'evidence must be a bounded regular file')
        raw = stream.read(maximum + 1)
        after = os.fstat(stream.fileno())
    run.need(len(raw) <= maximum, 'evidence grew beyond its byte bound')
    fields = lambda info: (info.st_dev, info.st_ino, info.st_size, info.st_mtime_ns, info.st_ctime_ns)
    run.need(fields(before) == fields(after), 'evidence changed while reading; capture a stable slice')
    return raw


def verified_report(controller_dir, raw, input_digests):
    cohort, _, cohort_digest, measurement = aggregate.verify_measurements(controller_dir, experiments.OVERLAP)
    journal = raw.decode('utf-8')
    arm_keys = {}
    for name in experiments.OVERLAP.phases:
        path = controller_dir / (name + '-attempts.json')
        attempts = qual.read_bounded_json_file(path)
        arm_keys[name] = [(qual.normalize_hex64(item['parsed_response']['session_id']['boot_nonce'], 'session boot'),
                           item['parsed_response']['session_id']['seq']) for item in attempts['attempts']]
    keys = [key for name in experiments.OVERLAP.phases for key in arm_keys[name]]
    run.need(len(keys) == 24 and len(set(keys)) == 24, 'phase report requires 24 unique validated response sessions')
    report = phases.correlate_phase_evidence(journal, keys)
    return {
        'schema': 'arc.community-shard-overlap.phase-report.v1',
        'workflow_run_id': cohort['workflow_run_id'], 'workflow_run_attempt': cohort['workflow_run_attempt'],
        'candidate_sha': cohort['candidate_sha'], 'coordinator_boot_nonce': cohort['coordinator_boot_nonce'],
        'cohort_sha256': cohort_digest, 'controller_files_sha256': input_digests,
        'journal_sha256': hashlib.sha256(raw).hexdigest(), 'journal_bytes': len(raw),
        'journal_authentication': 'caller_supplied_capture_not_independently_authenticated',
        'response_verification_basis': qual.VERIFICATION_BASIS,
        'counts_toward_operator_target': False, 'proves_server_admission_concurrency': False,
        'phase_records_complete': report['complete_ok_session_count'] == len(keys),
        'measurement': measurement, 'phase_evidence': report,
        'arms': {name: phases.correlate_phase_evidence(journal, arm_keys[name]) for name in experiments.OVERLAP.phases},
    }


def build_report(controller_dir, journal_path):
    names = ['cohort.json', 'cohort-digest.json', experiments.OVERLAP.measurement_file]
    names.extend(name + suffix for name in experiments.OVERLAP.phases for suffix in ('-attempts.json', '-arm.json'))
    journal = read_bounded_regular(journal_path, phases.MAX_JOURNAL_BYTES)
    with tempfile.TemporaryDirectory(prefix='arc-overlap-phase-') as temp:
        snapshot = Path(temp)
        digests = {}
        for name in names:
            raw = read_bounded_regular(controller_dir / name, qual.MAX_JSON_FILE_BYTES)
            descriptor = os.open(snapshot / name, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
            with os.fdopen(descriptor, 'wb') as stream:
                stream.write(raw)
            digests[name] = hashlib.sha256(raw).hexdigest()
        return verified_report(snapshot, journal, digests)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--controller-dir', type=Path, required=True)
    parser.add_argument('--journal-file', type=Path, required=True)
    parser.add_argument('--output-dir', type=Path, required=True)
    args = parser.parse_args()
    report = build_report(args.controller_dir, args.journal_file)
    run.write_new(args.output_dir / 'phase-timing-evidence.json', report)
    qual.verify_evidence_dir_no_secrets(args.output_dir)
    print(json.dumps({'phase_records_complete': report['phase_records_complete'],
                      'matched_sessions': report['phase_evidence']['requested_session_count'],
                      'unknown_sessions': report['phase_evidence']['unknown_session_count']}))


if __name__ == '__main__':
    main()
