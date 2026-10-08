"""Fresh-process original-source reference capture for resource measurement."""
import json
import sys
from pathlib import Path
from .compare import canonical
from .official import execute

if __name__ == '__main__':
    root = Path(sys.argv[1])
    raw = (root/'request.json').read_bytes()
    reference = execute(root/'source', root/'source/tiny-kimi-packed.source.json', root/'original', json.loads(raw), raw)
    (root/'reference.json').write_bytes(canonical(reference))
