import json
import os
from pathlib import Path
import select
import re
import signal
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

import community_shard_six_machine as qual
import scaling_controller as controller
import scaling_phase as phase
import scaling_run as run
from test_scaling_controller import BOOT_NONCE, make_dispatch_response, make_readiness, make_status, make_workers


class BoundaryTests(unittest.TestCase):
    def test_workflow_budgets_include_waiting_dispatch_cleanup_and_downloads(self):
        root = Path(os.environ.get('ARC_QUALIFICATION_SOURCE_ROOT', str(Path(qual.__file__).resolve().parents[2])))
        text = (root / '.github/workflows/community-shard-scaling.yml').read_text()
        worker = text.split('\n  shard-worker:\n', 1)[1].split('\n  controller:\n', 1)[0]
        control = text.split('\n  controller:\n', 1)[1].split('\n  aggregate:\n', 1)[0]
        self.assertEqual(re.findall(r'timeout-minutes: (\d+)', worker), ['80'])
        self.assertEqual(re.findall(r'timeout-minutes: (\d+)', control), ['60'])
        self.assertIn('max-parallel: 12', worker)
        slots = worker.split('        slot:\n', 1)[1].split('    steps:', 1)[0]
        self.assertEqual([int(value) for value in re.findall(r'- (\d+)', slots)], list(range(1, 13)))
        waves = sum(controller.REQUEST_COUNT // spec[2] for spec in controller.ARM_SPECS.values())
        experiment = run.INITIALIZATION_TIMEOUT_SECONDS + 3 * controller.READINESS_TIMEOUT_SECONDS + waves * controller.DISPATCH_TIMEOUT_SECONDS + phase.CLEANUP_SECONDS + 120
        self.assertLessEqual(experiment, phase.TOTAL_SECONDS)
        self.assertLessEqual(phase.TOTAL_SECONDS, qual.MAX_WORKER_SUPERVISOR_SECONDS)
        preparation = qual.MODEL_DOWNLOAD_TOTAL_SECONDS + qual.MODEL_DOWNLOAD_KILL_GRACE_SECONDS + qual.PREPARATION_GRACE_SECONDS
        self.assertLessEqual(preparation + phase.TOTAL_SECONDS + qual.RUNTIME_MAX_GRACE_SECONDS + qual.ARTIFACT_AND_CLEANUP_GRACE_SECONDS, 80 * 60)
        self.assertIn('scaling_phase.py worker', worker)
        self.assertNotIn('--max-seconds', worker)


    def test_owned_signal_wakes_worker_flag_without_a_thread_lock(self):
        code = "import signal,time;import community_shard_six_machine as q;f=q.SignalStopFlag();signal.signal(signal.SIGTERM,lambda s,t:f.set());print('ready',flush=True);started=time.monotonic();f.wait(10);print(f.is_set(),time.monotonic()-started,flush=True)"
        environment = dict(os.environ, PYTHONPATH=str(Path(qual.__file__).parent))
        child = subprocess.Popen([sys.executable, '-c', code], stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=environment)
        try:
            readable, _, _ = select.select([child.stdout], [], [], 10)
            self.assertTrue(readable)
            self.assertEqual(child.stdout.readline().strip(), 'ready')
            child.send_signal(signal.SIGTERM)
            stdout, stderr = child.communicate(timeout=3)
            self.assertEqual(child.returncode, 0, stderr)
            flag, elapsed = stdout.split()
            self.assertEqual(flag, 'True')
            self.assertLess(float(elapsed), 1)
        finally:
            if child.poll() is None:
                child.kill()
                child.communicate(timeout=3)

    def test_cross_arm_mismatch_or_reused_session_stops_before_the_next_wave(self):
        workers = make_workers(12)
        baseline = make_dispatch_response(workers[:6], seq=90)
        for reused in (False, True):
            with self.subTest(reused=reused), tempfile.TemporaryDirectory() as folder:
                responses = [make_dispatch_response(workers[:6], seq=90 if reused else 1, output='ok' if reused else 'different'), make_dispatch_response(workers[6:], seq=2, output='ok' if reused else 'different')]
                def post(*args, **kwargs):
                    return 200, responses.pop(0), 1
                def get(route, deadline):
                    return make_readiness() if route == qual.READINESS_PATH else make_status(12, 0, 0, 2)
                path = Path(folder) / 'attempts.json'
                with patch.object(qual, 'coordinator_get_json_with_retries', side_effect=get), patch.object(qual, 'coordinator_request_once', side_effect=post) as dispatch:
                    with self.assertRaises(qual.QualificationError):
                        controller.run_arm('expanded', set(workers), path, BOOT_NONCE, expected_baseline_response=baseline, prior_session_keys={(BOOT_NONCE, 90)})
                self.assertEqual(dispatch.call_count, 2)
                report = json.loads(path.read_text())
                self.assertEqual(report['arm_status'], 'failed')
                self.assertEqual(len(report['attempts']), 2)


if __name__ == '__main__':
    unittest.main()
