"""Measure one isolated child; unavailable Windows RSS/CPU times stay null."""
import json
import platform
import subprocess
import sys
import time
from pathlib import Path


def main():
    output, *command = sys.argv[1:]
    start = time.perf_counter()
    result = subprocess.run(command)
    elapsed = time.perf_counter() - start
    if sys.platform == 'win32':
        user = system = peak_rss = None
        resource_method = 'Windows: RSS and CPU times not measured (null); wall time only'
    else:
        import resource
        usage = resource.getrusage(resource.RUSAGE_CHILDREN)
        user, system = usage.ru_utime, usage.ru_stime
        peak_rss = usage.ru_maxrss * (1 if sys.platform == 'darwin' else 1024)
        resource_method = 'Unix getrusage peak RSS'
    data = {'schema': 'arc.fixture.process-resource.v1', 'command': command,
            'host': {'os': platform.system(), 'arch': platform.machine(), 'python': platform.python_version()},
            'wall_seconds': elapsed, 'user_seconds': user, 'system_seconds': system,
            'peak_rss_bytes': peak_rss,
            'exit_code': result.returncode,
            'setup': 'single fresh child; whole process including imports/load/capture/serialization; not inference-only timing; warm OS page cache uncontrolled; synthetic fixture; ' + resource_method}
    Path(output).write_text(json.dumps(data, indent=2)+'\n')
    raise SystemExit(result.returncode)


if __name__ == '__main__': main()
