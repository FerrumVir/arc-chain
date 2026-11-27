import json
import math
from pathlib import Path
import subprocess
import tempfile
import time

import community_shard_six_machine as qual


def request_once(method, path, timeout_seconds, json_body=None):
    if type(timeout_seconds) not in (int, float) or not math.isfinite(timeout_seconds) or not 0.001 <= timeout_seconds <= 310:
        raise qual.QualificationError('overlap transport requires a finite deadline between 1 ms and 310 seconds')
    deadline_ms = math.floor(timeout_seconds * 1000)
    timeout_text = f'{deadline_ms / 1000:.3f}'
    if method == 'POST':
        if path != qual.DISPATCH_PATH or json_body != qual.DISPATCH_PAYLOAD:
            raise qual.QualificationError('overlap POST must use the fixed qualified inference request')
    elif method != 'GET' or path not in (qual.SHARD_STATUS_PATH, qual.READINESS_PATH) or json_body is not None:
        raise qual.QualificationError('overlap transport refused an unsupported request')
    url = qual.COORDINATOR_URL + path
    qual.assert_pinned_https_url(url, qual.COORDINATOR_NETLOC)
    started = time.monotonic()
    with tempfile.TemporaryDirectory(prefix='arc-overlap-http-') as temp:
        destination = Path(temp) / 'response.json'
        command = ['/usr/bin/curl', '--disable', '--silent', '--show-error', '--noproxy', '*',
                   '--proto', '=https', '--proto-redir', '=https', '--tlsv1.2', '--max-redirs', '0',
                   '--connect-timeout', f'{min(15000, deadline_ms) / 1000:.3f}', '--max-time', timeout_text,
                   '--max-filesize', str(qual.MAX_HTTP_BODY_BYTES), '--request', method,
                   '--header', 'Accept: application/json', '--output', str(destination), '--write-out', '%{http_code}']
        payload = None
        if method == 'POST':
            command.extend(['--header', 'Content-Type: application/json', '--data-binary', '@-'])
            payload = json.dumps(json_body, separators=(',', ':')).encode()
        command.append(url)
        try:
            result = subprocess.run(command, input=payload, capture_output=True, timeout=timeout_seconds + 5,
                                    env={'PATH': '/usr/bin:/bin', 'LANG': 'C'})
        except subprocess.TimeoutExpired as exc:
            raise TimeoutError('owned overlap transport exceeded its total deadline and was stopped') from exc
        except OSError as exc:
            raise qual.QualificationError('overlap transport could not start; do not retry the POST') from exc
        if result.returncode == 28:
            raise TimeoutError('overlap request exceeded its transport deadline; do not retry the POST')
        if result.returncode != 0:
            raise qual.QualificationError(f'overlap transport failed with curl exit {result.returncode}; do not retry the POST')
        status_raw = result.stdout.strip()
        if len(status_raw) != 3 or not status_raw.isdigit():
            raise qual.QualificationError('overlap transport returned malformed HTTP status')
        status = int(status_raw)
        if status != 200:
            raise qual.QualificationError(f'overlap request returned HTTP {status}; do not retry the POST')
        if not destination.is_file():
            raise qual.QualificationError('overlap response body is empty; do not retry the POST')
        with destination.open('rb') as stream:
            raw = qual.read_bounded_bytes(stream, qual.MAX_HTTP_BODY_BYTES, 'overlap response')
        try:
            parsed = json.loads(raw.decode('utf-8'))
        except (UnicodeDecodeError, json.JSONDecodeError) as exc:
            raise qual.QualificationError('overlap response is not valid JSON; do not retry the POST') from exc
    return status, parsed, int((time.monotonic() - started) * 1000)


def get_json(path, deadline, monotonic_fn=time.monotonic):
    remaining = deadline - monotonic_fn()
    if remaining < 0.001:
        raise qual.QualificationError('overlap observation exceeded its deadline')
    try:
        _, body, _ = request_once('GET', path, min(15.0, remaining))
    except TimeoutError as exc:
        raise qual.QualificationError('overlap observation timed out') from exc
    return body
