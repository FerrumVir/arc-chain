#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
ARTIFACT="$REPO_ROOT/scripts/recovery/legacy-validator-set-40m.json"
SIDECAR="$ARTIFACT.sha256"

python3 - "$ARTIFACT" <<'PY'
import hashlib
import json
import pathlib
import re
import sys

path = pathlib.Path(sys.argv[1])
raw = path.read_bytes()
assert hashlib.sha256(raw).hexdigest() == "1615413b0cad59eedc8f9aa8ce41427e866f4b868f5b2148be48a1d722d7a3db"
rows = json.loads(raw)
expected = [
    "030eab8217c91526ee96c263fa40541fdee2be5b3bb808fb6bd5775175a9df2d",
    "0cda729e004c87fd15efc6b859ab567bbaba82ba95bdcf5f026082e0865e938e",
    "16e1afc6f6323be62e37a823f36568c9427baca08a7ed12ab289e08dffddb97d",
    "4f6f87d3fc2aac2b76778fa5d95cc72ff7b1f33c6c47abd3f277aeccc6833545",
    "868dddb80041cdaa7aaa0b2992f4dfc49628a26f2c7424c985310ed0bead7aba",
    "8bed4ea91365a9c92c67f2bb660ab8d39cb130d973eb6fb93cdb1dfdc4a9f3d3",
    "bc27a1a9a0a8f7fcadb8d60641170e58083b990fe92614041a044b5de724bd62",
    "c7a6141ddfe8ce668c3683b0097969cbab2d686494e1bae15bc464baa42264fd",
]
assert isinstance(rows, list) and len(rows) == 8
assert [row["address"] for row in rows] == expected
assert all(set(row) == {"address", "stake"} for row in rows)
assert all(re.fullmatch(r"[0-9a-f]{64}", row["address"]) for row in rows)
assert all(row["stake"] == 5_000_000 for row in rows)
assert sum(row["stake"] for row in rows) == 40_000_000
PY

expected_sidecar='1615413b0cad59eedc8f9aa8ce41427e866f4b868f5b2148be48a1d722d7a3db  legacy-validator-set-40m.json'
[ "$(tr -d '\r\n' < "$SIDECAR")" = "$expected_sidecar" ]

printf 'legacy validator set contract: PASS\n'
