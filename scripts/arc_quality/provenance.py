"""Content identities for evidence supplied by a run's producer.

These validate consistency, not the honesty of an external producer. An
audited manifest must identify the actual weights/revision used by the run.
Labels, model directory names and API aliases are not immutable identities.
"""

import hashlib
import json
import re


def digest(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(",", ":"),
                                     ensure_ascii=False, allow_nan=False).encode()).hexdigest()


def sha256(value):
    return isinstance(value, str) and re.fullmatch(r"[0-9a-f]{64}", value) is not None


def identity(provenance):
    if not isinstance(provenance, dict):
        return None
    keys = ("model_id", "model_revision", "weights_sha256", "tokenizer_sha256")
    if not all(isinstance(provenance.get(k), str) and provenance[k].strip() for k in keys):
        return None
    if not all(sha256(provenance[k]) for k in keys[2:]):
        return None
    return tuple(provenance[k] for k in keys)
