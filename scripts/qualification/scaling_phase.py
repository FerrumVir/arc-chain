import argparse
import os
from pathlib import Path
import signal
import subprocess
import sys
import time

import community_shard_six_machine as qual
import scaling_run as run

KINDS = ('cohort', 'expand', 'stop-added', 'done')
SCHEMA = 'arc.community-shard-scaling.phase.v1'
TOTAL_SECONDS = 3600
CLEANUP_SECONDS = 120


def artifact_name(workflow, attempt, kind):
    run.need(kind in KINDS, 'unknown scaling phase marker')
    return 'qual-scaling-' + kind + '-r' + qual.validate_positive_id(workflow, 'workflow') + '-a' + qual.validate_positive_id(attempt, 'attempt')


def marker_value(workflow, attempt, kind, digest):
    artifact_name(workflow, attempt, kind)
    if digest is None:
        run.need(kind == 'done', 'only abort completion may omit an unavailable cohort')
    else:
        run.need(qual.normalize_hex64(digest, 'cohort digest') == digest, 'cohort digest must be canonical')
    return {'schema': SCHEMA, 'workflow_run_id': workflow, 'workflow_run_attempt': attempt,
            'kind': kind, 'cohort_sha256': digest, 'counts_toward_operator_target': False}


def validate_marker(value, workflow, attempt, kind, digest):
    run.need(isinstance(value, dict), 'phase marker must be an object')
    run.need(value == marker_value(workflow, attempt, kind, digest), 'phase marker does not bind this run, attempt, phase and cohort')
    return value


def publish(args):
    directory = Path(args.cohort_dir)
    digest = None
    if (directory / 'cohort.json').exists():
        cohort, _, digest = run.read_cohort(directory)
        run.need(cohort['workflow_run_id'] == args.workflow_run_id and cohort['workflow_run_attempt'] == args.workflow_run_attempt, 'cohort belongs to another workflow')
    else:
        run.need(args.kind == 'done', 'initialize a verified cohort before phase publication')
    if args.kind in ('expand', 'stop-added'):
        phase = 'before' if args.kind == 'expand' else 'expanded'
        evidence = qual.read_bounded_json_file(directory / (phase + '-attempts.json'))
        run.need(evidence.get('arm_status') == 'succeeded', 'cannot publish next phase after failed measurement')
        _, identities, _ = run.read_cohort(directory)
        slots = qual.EXPECTED_SLOTS if phase == 'before' else qual.SCALING_SLOTS
        workers = {identities[slot]['worker_id'] for slot in slots}
        reconstructed = run.reconstruct_arm(phase, evidence, workers, cohort['coordinator_boot_nonce'])
        run.need(reconstructed == qual.read_bounded_json_file(directory / (phase + '-arm.json')), 'phase measurement differs from verified dispatch evidence')
    output = Path(args.output_dir)
    run.write_new(output / 'phase.json', marker_value(args.workflow_run_id, args.workflow_run_attempt, args.kind, digest))
    if args.kind == 'cohort':
        for name in ('cohort.json', 'cohort-digest.json'):
            descriptor = os.open(output / name, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
            with os.fdopen(descriptor, 'wb') as stream:
                stream.write((directory / name).read_bytes())
    qual.verify_evidence_dir_no_secrets(output)


class PhaseReader:
    def __init__(self, workflow, attempt, directory, token, deadline):
        self.workflow = qual.validate_positive_id(workflow, 'workflow')
        self.attempt = qual.validate_positive_id(attempt, 'attempt')
        self.directory = directory
        self.token = token
        self.deadline = deadline
        self.cache = {}

    def poll(self, kinds):
        metadata = run.list_artifacts(self.workflow, self.token, self.deadline - CLEANUP_SECONDS)
        values = {}
        for kind in kinds:
            name = artifact_name(self.workflow, self.attempt, kind)
            matches = [row for row in metadata if row.get('name') == name and row.get('expired') is False]
            run.need(len(matches) <= 1, 'duplicate phase artifact: ' + name)
            if not matches:
                run.need(kind not in self.cache, 'verified phase artifact disappeared')
                continue
            row = matches[0]
            if kind in self.cache:
                run.need((row['id'], row['digest']) == self.cache[kind][0], 'phase artifact changed after verification')
                values[kind] = self.cache[kind][1]
                continue
            remaining = int(self.deadline - CLEANUP_SECONDS - time.monotonic())
            run.need(remaining > 0, 'phase artifact wait exceeded the worker deadline')
            target = self.directory / kind
            qual.download_validated_artifact_by_id(self.workflow, name, row, target, qual.MAX_IDENTITY_ARTIFACT_BYTES, self.token, timeout_seconds=min(60, remaining), max_files=4)
            value = qual.read_bounded_json_file(target / 'phase.json')
            self.cache[kind] = ((row['id'], row['digest']), value)
            values[kind] = value
        return values


def remaining_runtime(deadline, now):
    remaining = int(deadline - now) - CLEANUP_SECONDS
    run.need(remaining >= 60, 'insufficient time remains to start a bounded worker and clean it up')
    return remaining


def stop_child(child, deadline=None):
    if child.poll() is None:
        child.send_signal(signal.SIGTERM)
    timeout = CLEANUP_SECONDS if deadline is None else min(CLEANUP_SECONDS, max(0, deadline - time.monotonic()))
    return child.wait(timeout=timeout)


def child_command(args, deadline):
    command = [sys.executable, str(Path(qual.__file__).resolve()), 'worker-run']
    for key in ('workflow_run_id', 'workflow_run_attempt', 'slot', 'candidate_dir', 'model_path', 'private_key_path', 'identity_dir', 'work_dir', 'evidence_dir'):
        command.extend(['--' + key.replace('_', '-'), str(getattr(args, key))])
    command.extend(['--max-seconds', str(remaining_runtime(deadline, time.monotonic()))])
    return command


def validate_clean_worker(evidence, identity):
    run.need(isinstance(evidence, dict) and isinstance(identity, dict), 'worker identity and cleanup evidence must be objects')
    for key in ('worker_id', 'worker_name', 'boot_id', 'hostname', 'runner_name', 'candidate_sha', 'candidate_run_id', 'candidate_run_attempt', 'workflow_run_id', 'workflow_run_attempt', 'slot'):
        run.need(key in identity and key in evidence and evidence[key] == identity[key], 'worker cleanup identity mismatch: ' + key)
    run.need(type(evidence.get('exit_code')) is int and evidence['exit_code'] == 0, 'worker process did not exit cleanly')
    run.need(evidence.get('key_deleted') is True, 'worker private key was not deleted')
    run.need(evidence.get('parse_error') is None and evidence.get('supervisor_error') is None, 'worker supervisor did not prove clean exit')
    run.need(evidence.get('stdout_truncated') is False and evidence.get('stderr_truncated') is False, 'worker evidence logs were truncated')
    summary = qual.validate_exit_report(evidence.get('exit_report'), identity['worker_id'], identity['worker_name'])['summary']
    for key in ('registrations', 'leases_acquired', 'models_loaded', 'heartbeats_sent', 'steps_executed', 'steps_submitted'):
        run.need(summary[key] > 0, 'worker has no measured ' + key)
    run.need(summary['submit_stale_conflicts'] == 0 and summary['clean_release_sent'] is True, 'worker did not release its assignment cleanly')
    memory = evidence.get('systemd_memory_peak_bytes')
    run.need(type(memory) is int and 0 < memory <= 8 * 1024**3, 'worker peak memory is missing or exceeds its budget')
    return summary


def supervise(args):
    deadline = time.monotonic() + TOTAL_SECONDS
    slot = qual.validate_slot(args.slot)
    directory = Path(args.evidence_dir)
    directory.mkdir(parents=True, exist_ok=True)
    receipt_path = directory / 'scaling-supervisor.json'
    run.need(not receipt_path.exists(), 'worker already supervised; never repeat this measurement')
    reader = PhaseReader(args.workflow_run_id, args.workflow_run_attempt, Path(args.work_dir) / 'phases', os.environ.get('GH_TOKEN', ''), deadline)
    child = None
    digest = None
    stop_kind = None
    exit_code = None
    failure = None
    completed = False
    stop_attempted = False
    previous = {}
    stopped = qual.SignalStopFlag()

    def on_signal(signum, frame):
        stopped.set()

    try:
        identity = qual.read_bounded_json_file(Path(args.identity_dir) / 'worker-identity.json')
        run.need(isinstance(identity, dict), 'worker identity must be an object')
        run.need(identity.get('slot') == slot and identity.get('workflow_run_id') == args.workflow_run_id and identity.get('workflow_run_attempt') == args.workflow_run_attempt, 'local worker identity belongs to another slot or run')
        for signum in (signal.SIGINT, signal.SIGTERM):
            previous[signum] = signal.signal(signum, on_signal)
        while child is None:
            run.need(not stopped.is_set(), 'supervisor interrupted before worker start')
            remaining_runtime(deadline, time.monotonic())
            phases = reader.poll(('done', 'cohort', 'expand', 'stop-added') if slot > 6 else ('done', 'cohort'))
            if 'done' in phases:
                value = phases['done']
                run.need(isinstance(value, dict), 'done phase must be an object')
                validate_marker(value, args.workflow_run_id, args.workflow_run_attempt, 'done', digest if digest is not None else value.get('cohort_sha256'))
                stop_kind = 'done-before-start'
                raise qual.QualificationError('controller finished before this worker started')
            if 'stop-added' in phases:
                run.need(isinstance(phases['stop-added'], dict), 'stop-added phase must be an object')
                validate_marker(phases['stop-added'], args.workflow_run_id, args.workflow_run_attempt, 'stop-added', digest if digest is not None else phases['stop-added'].get('cohort_sha256'))
                stop_kind = 'stop-added-before-start'
                raise qual.QualificationError('added cohort was stopped before this worker started')
            if 'cohort' in phases:
                cohort, identities, digest = run.read_cohort(reader.directory / 'cohort')
                run.need(cohort['workflow_run_id'] == args.workflow_run_id and cohort['workflow_run_attempt'] == args.workflow_run_attempt, 'published cohort belongs to another workflow')
                validate_marker(phases['cohort'], args.workflow_run_id, args.workflow_run_attempt, 'cohort', digest)
                run.need(identities[slot] == identity, 'published cohort does not contain this exact worker identity')
                if slot <= 6 or 'expand' in phases:
                    if slot > 6:
                        validate_marker(phases['expand'], args.workflow_run_id, args.workflow_run_attempt, 'expand', digest)
                    child = subprocess.Popen(child_command(args, deadline), stdin=subprocess.DEVNULL)
                    break
            stopped.wait(min(30, max(0, deadline - time.monotonic() - CLEANUP_SECONDS)))
        while child.poll() is None:
            run.need(not stopped.is_set(), 'supervisor interrupted during measurement')
            run.need(time.monotonic() < deadline - CLEANUP_SECONDS, 'worker reached the global experiment deadline')
            kinds = ('stop-added', 'done') if slot > 6 else ('done',)
            phases = reader.poll(kinds)
            for kind in kinds:
                if kind in phases:
                    validate_marker(phases[kind], args.workflow_run_id, args.workflow_run_attempt, kind, digest)
                    stop_kind = kind
                    stop_attempted = True
                    exit_code = stop_child(child, deadline)
                    break
            if stop_kind is not None:
                break
            stopped.wait(min(15, max(0, deadline - time.monotonic() - CLEANUP_SECONDS)))
        if exit_code is None:
            exit_code = child.poll()
        run.need(stop_kind == ('done' if slot <= 6 else 'stop-added'), 'worker exited without its verified phase completion')
        evidence = qual.read_bounded_json_file(directory / 'worker-evidence.json')
        run.need(isinstance(evidence, dict), 'worker evidence must be an object')
        run.need(evidence.get('stop_reason') == 'signal_15', 'base supervisor stopped for an unexpected reason')
        run.need(exit_code == 1, 'base supervisor exit must report its intentional externally signalled stop')
        validate_clean_worker(evidence, identity)
        completed = True
    except (qual.QualificationError, OSError, subprocess.SubprocessError, ValueError, KeyError, TypeError) as error:
        failure = qual.redact_secret(str(error), os.environ.get('GH_TOKEN', ''))
    finally:
        if child is not None and child.poll() is None and not stop_attempted:
            try:
                stop_attempted = True
                exit_code = stop_child(child, deadline)
            except (subprocess.TimeoutExpired, OSError):
                failure = (failure + '; ' if failure else '') + 'owned child failed to finish its bounded cleanup; workflow unit cleanup is required'
        if not completed and failure is None:
            failure = 'aborted before all proof checks completed'
        deleted = qual.delete_private_key(Path(args.private_key_path))
        for signum, handler in previous.items():
            signal.signal(signum, handler)
        run.write_new(receipt_path, {'schema': 'arc.community-shard-scaling.supervisor.v1', 'slot': slot,
                                   'cohort_sha256': digest, 'stop_kind': stop_kind, 'key_deleted': deleted,
                                   'child_supervisor_exit_code': exit_code, 'passed': completed and failure is None and deleted,
                                   'error': failure, 'counts_toward_operator_target': False})
    run.need(completed and failure is None and deleted, failure or 'private key cleanup failed')


def main():
    parser = argparse.ArgumentParser()
    commands = parser.add_subparsers(dest='command', required=True)
    marker = commands.add_parser('publish')
    marker.add_argument('kind', choices=KINDS)
    for key in ('workflow-run-id', 'workflow-run-attempt', 'cohort-dir', 'output-dir'):
        marker.add_argument('--' + key, required=True)
    marker.set_defaults(function=publish)
    worker = commands.add_parser('worker')
    for key in ('workflow-run-id', 'workflow-run-attempt', 'candidate-dir', 'model-path', 'private-key-path', 'identity-dir', 'work-dir', 'evidence-dir'):
        worker.add_argument('--' + key, required=True)
    worker.add_argument('--slot', type=int, required=True)
    worker.set_defaults(function=supervise)
    args = parser.parse_args()
    args.function(args)


if __name__ == '__main__':
    main()
