from pathlib import Path
import subprocess
import unittest
from unittest.mock import patch

import community_shard_six_machine as qual
import overlap_transport as transport


class OverlapTransportTests(unittest.TestCase):
    def test_one_post_has_total_deadline_no_redirect_retry_or_ambient_configuration(self):
        locations = []
        def invoke(command, **options):
            self.assertEqual(command[:2], ['/usr/bin/curl', '--disable'])
            self.assertEqual(command[command.index('--max-time') + 1], '310')
            self.assertEqual(command[command.index('--max-redirs') + 1], '0')
            self.assertEqual(command[command.index('--proto') + 1], '=https')
            self.assertNotIn('--retry', command)
            self.assertNotIn('--location', command)
            self.assertNotIn('--insecure', command)
            self.assertEqual(options['timeout'], 315)
            self.assertNotIn('GH_TOKEN', options['env'])
            self.assertEqual(options['input'], b'{"input":"Hello","max_tokens":2,"chat_template":false}')
            destination = Path(command[command.index('--output') + 1])
            locations.append(destination)
            destination.write_text('{"synthetic":true}')
            return subprocess.CompletedProcess(command, 0, b'200', b'')
        with patch.object(transport.subprocess, 'run', side_effect=invoke) as call:
            status, result, _ = transport.request_once('POST', qual.DISPATCH_PATH, 310, qual.DISPATCH_PAYLOAD)
        call.assert_called_once()
        self.assertEqual((status, result), (200, {'synthetic': True}))
        self.assertFalse(locations[0].parent.exists())

    def test_http_errors_timeouts_and_process_failures_never_retry(self):
        for status, exit_code in ((b'429', 0), (b'502', 0), (b'503', 0), (b'504', 0), (b'000', 28), (b'000', 7)):
            with self.subTest(status=status, exit_code=exit_code), patch.object(transport.subprocess, 'run', return_value=subprocess.CompletedProcess([], exit_code, status, b'')) as call:
                with self.assertRaises((qual.QualificationError, TimeoutError)):
                    transport.request_once('POST', qual.DISPATCH_PATH, 310, qual.DISPATCH_PAYLOAD)
                call.assert_called_once()
        with patch.object(transport.subprocess, 'run', side_effect=subprocess.TimeoutExpired('curl', 315)) as call:
            with self.assertRaises(TimeoutError):
                transport.request_once('POST', qual.DISPATCH_PATH, 310, qual.DISPATCH_PAYLOAD)
            call.assert_called_once()

    def test_invalid_scope_and_expired_deadline_refuse_before_network(self):
        with patch.object(transport.subprocess, 'run') as call:
            for timeout in (True, 0, -1, 311, float('inf'), float('nan')):
                with self.assertRaises(qual.QualificationError):
                    transport.request_once('GET', qual.SHARD_STATUS_PATH, timeout)
            for method, path, body in [('POST', '/faucet/claim', {}), ('POST', qual.DISPATCH_PATH, {}), ('GET', '/account/a', None)]:
                with self.assertRaises(qual.QualificationError):
                    transport.request_once(method, path, 10, body)
            with self.assertRaisesRegex(qual.QualificationError, 'deadline'):
                transport.get_json(qual.SHARD_STATUS_PATH, 0)
            call.assert_not_called()


if __name__ == '__main__':
    unittest.main()
