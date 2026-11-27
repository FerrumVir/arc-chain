import argparse
import hashlib
import json
import os
from pathlib import Path
import time

import community_shard_six_machine as qual
import scaling_benchmark
import scaling_controller
import experiment_profile as experiments

SCHEMA = 'arc.community-shard-scaling.cohort.v1'
PHASES = ('before', 'expanded', 'after')
INITIALIZATION_TIMEOUT_SECONDS = 900


def need(condition, message):
    if not condition:
        raise qual.QualificationError(message)


def write_new(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    raw = (json.dumps(value, sort_keys=True, indent=2) + '\n').encode()
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    with os.fdopen(descriptor, 'wb') as stream:
        stream.write(raw)
        stream.flush()
        os.fsync(stream.fileno())
    return hashlib.sha256(raw).hexdigest()


def list_artifacts(workflow, token, deadline):
    found = {}
    for page in range(1, qual.MAX_GH_ARTIFACT_PAGES + 1):
        remaining = deadline - time.monotonic()
        need(remaining > 0, 'artifact listing exceeded its phase deadline')
        payload = qual.github_api_get_json('/repos/' + qual.EXPECTED_REPO + '/actions/runs/' + workflow + '/artifacts?per_page=100&page=' + str(page), token, timeout_seconds=min(15, remaining))
        need(isinstance(payload, dict) and isinstance(payload.get('artifacts'), list), 'GitHub artifact listing must contain an artifacts array')
        for item in payload['artifacts']:
            need(isinstance(item, dict), 'GitHub artifact metadata must be an object')
            if item.get('expired') is not False:
                continue
            name = item.get('name')
            need(isinstance(name, str), 'GitHub artifact name is missing')
            artifact_id = item.get('id')
            need(type(artifact_id) is int and artifact_id > 0, 'GitHub artifact ID is missing')
            if artifact_id in found:
                need(found[artifact_id] == item, 'artifact metadata changed during pagination')
            else:
                found[artifact_id] = item
        if len(payload['artifacts']) < 100:
            return list(found.values())
    raise qual.QualificationError('artifact pagination limit reached; refuse an incomplete evidence inventory')


def initialize(args):
    profile = experiments.select(getattr(args, 'mode', 'scaling'))
    out = Path(args.output_dir)
    deadline = time.monotonic() + INITIALIZATION_TIMEOUT_SECONDS
    need(not (out / 'cohort.json').exists(), 'cohort already initialized; use this run evidence without restarting phases')
    run = qual.validate_positive_id(args.workflow_run_id, 'workflow_run_id')
    attempt = qual.validate_positive_id(args.workflow_run_attempt, 'workflow_run_attempt')
    candidate = qual.validate_positive_id(args.candidate_run_id, 'candidate_run_id')
    candidate_attempt = qual.validate_positive_id(args.candidate_run_attempt, 'candidate_run_attempt')
    source = qual.normalize_git_sha(args.candidate_sha)
    token = os.environ.get('GH_TOKEN', '')
    qual.validate_candidate_run_api(candidate, candidate_attempt, source, token)
    names = {slot: qual.make_identity_artifact_name(run, attempt, slot) for slot in qual.SCALING_SLOTS}
    while True:
        artifacts = list_artifacts(run, token, deadline)
        found = {}
        for slot, name in names.items():
            matches = [row for row in artifacts if row.get('name') == name and row.get('expired') is False]
            need(len(matches) <= 1, 'duplicate identity artifact for slot ' + str(slot))
            if matches:
                found[slot] = matches[0]
        if len(found) == 12:
            break
        remaining = deadline - time.monotonic()
        need(remaining > 0, 'twelve-machine identity wait expired; missing slots ' + str(sorted(set(names) - set(found))))
        time.sleep(min(10, remaining))
    identities = []
    for slot in qual.SCALING_SLOTS:
        directory = out / ('identity-slot-' + str(slot))
        remaining = int(deadline - time.monotonic())
        need(remaining > 0, 'identity transfer exceeded the initialization deadline')
        qual.download_validated_artifact_by_id(run, names[slot], found[slot], directory, qual.MAX_IDENTITY_ARTIFACT_BYTES, token, timeout_seconds=min(120, remaining), max_files=4)
        identities.append(qual.read_bounded_json_file(directory / 'worker-identity.json'))
    normalized = qual.validate_worker_identities(identities, run, attempt, candidate, candidate_attempt, source, expected_slots=qual.SCALING_SLOTS)
    status = qual.coordinator_get_json_with_retries(qual.SHARD_STATUS_PATH, time.monotonic() + 30)
    qual.assert_status_no_external_interference(status, max_workers=0)
    boot = qual.normalize_hex64(status['status']['boot_nonce'], 'coordinator boot nonce')
    scaling_controller._verify_status_pins_and_health(status, boot)
    cohort = {'schema': profile.schema('cohort'), 'workflow_run_id': run, 'workflow_run_attempt': attempt,
              'candidate_run_id': candidate, 'candidate_run_attempt': candidate_attempt, 'candidate_sha': source,
              'coordinator_boot_nonce': boot, 'identities': [normalized[slot] for slot in qual.SCALING_SLOTS],
              'classification': 'ephemeral_ci', 'counts_toward_operator_target': False,
              'identity_artifacts': {str(slot): {'name': names[slot], 'id': found[slot]['id'], 'digest': found[slot]['digest']} for slot in qual.SCALING_SLOTS},
              'initial_status': status}
    digest = write_new(out / 'cohort.json', cohort)
    write_new(out / 'cohort-digest.json', {'cohort_sha256': digest})
    print(json.dumps({'cohort_sha256': digest, 'distinct_machines': 12, 'counts_toward_operator_target': False}))


def read_cohort(directory, profile=experiments.SCALING):
    path = directory / 'cohort.json'
    need(path.is_file() and not path.is_symlink(), 'cohort must be a regular local evidence file')
    with path.open('rb') as stream:
        raw = stream.read(qual.MAX_JSON_FILE_BYTES + 1)
    need(len(raw) <= qual.MAX_JSON_FILE_BYTES, 'cohort exceeds evidence bound')
    cohort = json.loads(raw)
    need(cohort.get('schema') == profile.schema('cohort') and cohort.get('counts_toward_operator_target') is False, 'invalid experiment cohort scope')
    expected = qual.read_bounded_json_file(directory / 'cohort-digest.json')['cohort_sha256']
    need(hashlib.sha256(raw).hexdigest() == expected, 'cohort bytes changed after initialization')
    identities = qual.validate_worker_identities(cohort['identities'], cohort['workflow_run_id'], cohort['workflow_run_attempt'], cohort['candidate_run_id'], cohort['candidate_run_attempt'], cohort['candidate_sha'], expected_slots=qual.SCALING_SLOTS)
    return cohort, identities, expected


def validate_phase_order(directory, name, profile=experiments.SCALING):
    index = profile.phase_index(name)
    for phase in profile.phases[:index]:
        need((directory / (phase + '-attempts.json')).is_file(), 'previous arm evidence missing: ' + phase)
        report = qual.read_bounded_json_file(directory / (phase + '-attempts.json'))
        need(report.get('arm_status') == 'succeeded', 'previous arm did not succeed: ' + phase)
        need((directory / (phase + '-arm.json')).is_file(), 'previous arm measurement missing: ' + phase)
    need(not any((directory / (phase + suffix)).exists() or (directory / (phase + suffix)).is_symlink() for phase in profile.phases[index:] for suffix in ('-arm.json', '-attempts.json')), 'phase already attempted or later phase evidence exists; do not repeat inference POSTs')
    need(not (directory / profile.measurement_file).exists(), 'experiment measurement already exists')


def compare_phase_responses(directory, phases, profile=experiments.SCALING):
    sessions = set()
    baseline = None
    for phase in phases:
        report = qual.read_bounded_json_file(directory / (phase + '-attempts.json'))
        need(report.get('arm_status') == 'succeeded' and len(report['attempts']) == profile.requests(phase), 'incomplete arm evidence: ' + phase)
        for attempt in report['attempts']:
            need(attempt['status'] == 'succeeded' and attempt['dispatch_uncertain'] is False, 'uncertain or failed dispatch cannot count')
            response = attempt['parsed_response']
            session = (qual.normalize_hex64(response['session_id']['boot_nonce'], 'session boot'), response['session_id']['seq'])
            need(session not in sessions, 'duplicate coordinator session across scaling arms')
            sessions.add(session)
            if baseline is None:
                baseline = response
            else:
                difference = scaling_controller._check_consistency(baseline, response, 'first measured request', attempt['id'])
                if difference is not None:
                    raise difference


def reconstruct_arm(name, report, expected_workers, boot, profile=experiments.SCALING):
    need(report.get('arm_status') == 'succeeded', 'arm did not succeed: ' + name)
    phase_index = profile.phase_index(name)
    if profile is experiments.OVERLAP:
        need(report.get('experiment_mode') == 'overlap' and report.get('max_concurrency') == profile.offered_caps[phase_index], 'overlap attempt evidence has wrong experiment or offered cap')
    attempts = report.get('attempts')
    need(isinstance(attempts, list) and len(attempts) == profile.requests(name), 'arm must contain its exact dispatch attempt count')
    requests = []
    for index, attempt in enumerate(attempts, 1):
        need(attempt.get('id') == name + '-' + str(index), 'attempt identity or order changed')
        need(attempt.get('status') == 'succeeded' and attempt.get('dispatch_uncertain') is False, 'failed or uncertain POST cannot count')
        need(type(attempt.get('http_status')) is int and attempt['http_status'] == 200, 'dispatch HTTP result is not success')
        need(attempt.get('request_payload') == qual.DISPATCH_PAYLOAD, 'request payload differs between scaling arms')
        response = attempt.get('parsed_response')
        participants = scaling_controller._extract_participant_subset(response, expected_workers)
        validated = qual.validate_dispatch_response(response, participants)
        scaling_controller._verify_boot_nonce(validated['session_id']['boot_nonce'], 'session boot nonce', boot)
        requests.append({'id': attempt['id'], 'start_ns': attempt['start_ns'], 'end_ns': attempt['end_ns'],
                         'success': True, 'output_hash': qual.normalize_hex64(validated['output_hash'], 'output hash'),
                         'worker_ids': sorted(participants), 'server_verification_validated': True})
    return {'workers': sorted(expected_workers), 'requests': requests}


def measure(args):
    profile = experiments.select(getattr(args, 'mode', 'scaling'))
    directory = Path(args.output_dir)
    cohort, identities, _ = read_cohort(directory, profile)
    validate_phase_order(directory, args.name, profile)
    slots = profile.slots(args.name)
    workers = {identities[slot]['worker_id'] for slot in slots}
    prior_phases = profile.phases[:profile.phase_index(args.name)]
    baseline_response = None
    prior_sessions = set()
    for previous_phase in prior_phases:
        previous_slots = profile.slots(previous_phase)
        previous_workers = {identities[slot]['worker_id'] for slot in previous_slots}
        report = qual.read_bounded_json_file(directory / (previous_phase + '-attempts.json'))
        reconstructed = reconstruct_arm(previous_phase, report, previous_workers, cohort['coordinator_boot_nonce'], profile)
        need(reconstructed == qual.read_bounded_json_file(directory / (previous_phase + '-arm.json')), 'previous phase measurement changed')
        for attempt in report['attempts']:
            response = attempt['parsed_response']
            baseline_response = response if baseline_response is None else baseline_response
            prior_sessions.add((qual.normalize_hex64(response['session_id']['boot_nonce'], 'prior session boot'), response['session_id']['seq']))
    compare_phase_responses(directory, prior_phases, profile)
    if profile is experiments.OVERLAP:
        from overlap_controller import run_overlap_arm
        execute = run_overlap_arm
    else:
        execute = scaling_controller.run_arm
    arm = execute(args.name, workers, directory / (args.name + '-attempts.json'), cohort['coordinator_boot_nonce'], expected_baseline_response=baseline_response, prior_session_keys=prior_sessions)
    compare_phase_responses(directory, profile.phases[:profile.phase_index(args.name) + 1], profile)
    write_new(directory / (args.name + '-arm.json'), arm)
    if args.name == profile.phases[-1]:
        arms = [qual.read_bounded_json_file(directory / (phase + '-arm.json')) for phase in profile.phases]
        analyze = scaling_benchmark.analyze_overlap if profile is experiments.OVERLAP else scaling_benchmark.analyze_scaling
        summary = analyze(*arms)
        write_new(directory / profile.measurement_file, summary)
        print(json.dumps(summary))
    else:
        print(json.dumps({'arm': args.name, 'completed_requests': len(arm['requests']), 'workers': len(arm['workers'])}))


def main():
    parser = argparse.ArgumentParser()
    commands = parser.add_subparsers(dest='command', required=True)
    initialize_parser = commands.add_parser('initialize')
    for key in ('workflow-run-id', 'workflow-run-attempt', 'candidate-run-id', 'candidate-run-attempt', 'candidate-sha', 'output-dir'):
        initialize_parser.add_argument('--' + key, required=True)
    initialize_parser.set_defaults(function=initialize)
    initialize_parser.add_argument('--mode', choices=('scaling', 'overlap'), default='scaling')
    measure_parser = commands.add_parser('arm')
    measure_parser.add_argument('name', choices=(*experiments.SCALING.phases, *experiments.OVERLAP.phases))
    measure_parser.add_argument('--mode', choices=('scaling', 'overlap'), default='scaling')
    measure_parser.add_argument('--output-dir', required=True)
    measure_parser.set_defaults(function=measure)
    args = parser.parse_args()
    args.function(args)


if __name__ == '__main__':
    main()
