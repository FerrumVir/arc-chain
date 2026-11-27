import argparse
import os
import time
from pathlib import Path

import community_shard_six_machine as qual
import scaling_benchmark
import scaling_controller as controller
import scaling_phase as phase
import scaling_run as run


def verify_measurements(directory):
    cohort, identities, digest = run.read_cohort(directory)
    arms = []
    for name in run.PHASES:
        slots = qual.SCALING_SLOTS if name == 'expanded' else qual.EXPECTED_SLOTS
        workers = {identities[slot]['worker_id'] for slot in slots}
        report = qual.read_bounded_json_file(directory / (name + '-attempts.json'))
        arm = run.reconstruct_arm(name, report, workers, cohort['coordinator_boot_nonce'])
        run.need(arm == qual.read_bounded_json_file(directory / (name + '-arm.json')), 'saved timing summary differs from raw attempts: ' + name)
        arms.append(arm)
    run.compare_phase_responses(directory, run.PHASES)
    measurement = scaling_benchmark.analyze_scaling(*arms)
    run.need(measurement == qual.read_bounded_json_file(directory / 'scaling-measurement.json'), 'saved scaling measurement differs from recomputed result')
    return cohort, identities, digest, measurement


def download_one(workflow, name, artifacts, destination, token, maximum=8 * 1024**2):
    matches = [item for item in artifacts if item.get('name') == name and item.get('expired') is False]
    run.need(len(matches) == 1, 'expected exactly one complete artifact: ' + name)
    qual.download_validated_artifact_by_id(workflow, name, matches[0], destination, maximum, token, timeout_seconds=60, max_files=128)
    return {'name': name, 'id': matches[0]['id'], 'digest': matches[0]['digest']}


def main():
    parser = argparse.ArgumentParser()
    for key in ('workflow-run-id', 'workflow-run-attempt', 'candidate-run-id', 'candidate-run-attempt', 'candidate-sha', 'output-dir'):
        parser.add_argument('--' + key, required=True)
    args = parser.parse_args()
    workflow = qual.validate_positive_id(args.workflow_run_id, 'workflow')
    attempt = qual.validate_positive_id(args.workflow_run_attempt, 'attempt')
    args.candidate_run_id = qual.validate_positive_id(args.candidate_run_id, 'candidate run')
    args.candidate_run_attempt = qual.validate_positive_id(args.candidate_run_attempt, 'candidate attempt')
    args.candidate_sha = qual.normalize_git_sha(args.candidate_sha)
    token = os.environ.get('GH_TOKEN', '')
    directory = Path(args.output_dir)
    directory.mkdir(parents=True, exist_ok=True)
    metadata = run.list_artifacts(workflow, token, time.monotonic() + 120)
    provenance = []
    controller_dir = directory / 'controller'
    name = qual.make_controller_evidence_artifact_name(workflow, attempt)
    provenance.append(download_one(workflow, name, metadata, controller_dir, token))
    cohort, identities, digest, measurement = verify_measurements(controller_dir)
    for field, value in (('workflow_run_id', workflow), ('workflow_run_attempt', attempt), ('candidate_run_id', args.candidate_run_id), ('candidate_run_attempt', args.candidate_run_attempt), ('candidate_sha', args.candidate_sha)):
        run.need(cohort[field] == value, 'cohort provenance mismatch: ' + field)
    workers = []
    for slot in qual.SCALING_SLOTS:
        target = directory / ('worker-' + str(slot))
        name = qual.make_worker_evidence_artifact_name(workflow, attempt, slot)
        provenance.append(download_one(workflow, name, metadata, target, token))
        supervisor = qual.read_bounded_json_file(target / 'scaling-supervisor.json')
        expected_kind = 'done' if slot <= 6 else 'stop-added'
        run.need(supervisor.get('passed') is True and supervisor.get('key_deleted') is True and supervisor.get('error') is None, 'worker supervisor did not pass cleanup')
        run.need(supervisor.get('slot') == slot and supervisor.get('cohort_sha256') == digest, 'worker supervisor cohort mismatch')
        run.need(supervisor.get('stop_kind') == expected_kind and supervisor.get('counts_toward_operator_target') is False, 'worker supervisor has wrong scope or phase')
        evidence = qual.read_bounded_json_file(target / 'worker-evidence.json')
        run.need(evidence.get('stop_reason') == 'signal_15' and supervisor.get('child_supervisor_exit_code') == 1, 'unexpected worker stop semantics')
        summary = phase.validate_clean_worker(evidence, identities[slot])
        workers.append({'slot': slot, 'worker_id': identities[slot]['worker_id'], 'boot_id': identities[slot]['boot_id'],
                        'runner_name': identities[slot]['runner_name'], 'steps_executed': summary['steps_executed'],
                        'steps_submitted': summary['steps_submitted'], 'clean_release_sent': True,
                        'systemd_memory_peak_bytes': evidence['systemd_memory_peak_bytes']})
    for kind in ('cohort', 'expand', 'stop-added', 'done'):
        target = directory / ('marker-' + kind)
        provenance.append(download_one(workflow, phase.artifact_name(workflow, attempt, kind), metadata, target, token))
        phase.validate_marker(qual.read_bounded_json_file(target / 'phase.json'), workflow, attempt, kind, digest)
    readiness, status, error = qual.poll_coordinator_zero_after_cleanup(65)
    run.need(error is None, 'coordinator cleanup failed: ' + str(error))
    controller._verify_status_pins_and_health(status, cohort['coordinator_boot_nonce'])
    summary = {'schema': 'arc.community-shard-scaling.qualification.v1', 'overall_status': 'passed',
               'workflow_run_id': workflow, 'workflow_run_attempt': attempt, 'candidate_sha': args.candidate_sha,
               'cohort_sha256': digest, 'classification': 'ephemeral_ci', 'counts_toward_operator_target': False,
               'distinct_runner_machines': len(workers), 'completed_requests': 18,
               'verification_basis': qual.VERIFICATION_BASIS, 'measurement': measurement, 'workers': workers,
               'cleanup_readiness': readiness, 'cleanup_status': status, 'artifacts': provenance}
    run.write_new(directory / 'qualification-summary.json', summary)
    qual.verify_evidence_dir_no_secrets(directory)
    print(summary['overall_status'])


if __name__ == '__main__':
    main()
