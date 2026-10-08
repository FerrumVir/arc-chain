"""Measure one isolated child command on Unix; no aggregate/misattributed RSS."""
import json
import platform
import resource
import subprocess
import sys
import time
from pathlib import Path


def main():
    output, *command = sys.argv[1:]
    start = time.perf_counter()
    result = subprocess.run(command)
    elapsed = time.perf_counter() - start
    usage = resource.getrusage(resource.RUSAGE_CHILDREN)
    data = {'schema': 'arc.fixture.process-resource.v1', 'command': command,
            'host': {'os': platform.system(), 'arch': platform.machine(), 'python': platform.python_version()},
            'wall_seconds': elapsed, 'user_seconds': usage.ru_utime, 'system_seconds': usage.ru_stime,
            'peak_rss_bytes': usage.ru_maxrss * (1 if sys.platform == 'darwin' else 1024),
            'exit_code': result.returncode,
            'setup': 'single fresh child; whole process including imports/load/capture/serialization; not inference-only timing; warm OS page cache uncontrolled; synthetic fixture; Unix getrusage peak RSS'}
    Path(output).write_text(json.dumps(data, indent=2)+'\n')
    raise SystemExit(result.returncode)


if __name__ == '__main__': main()
