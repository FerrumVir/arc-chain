"""Fast tests for the orchestrator's pure helpers. No nodes."""

import os
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(HERE)))

from arc_soak import orchestrate  # noqa: E402

REAL_RECORD = """=== ARC build provenance ===
recorded_utc:     20260921T145417Z
command:          cargo build -p arc-node --locked
workdir:          /Users/excaulibur/work/arc-chain-readiness-20260919
source_revision:  a283e9dfa7c2c1257886eb406165c305c9a15eda
dirty_files:      0
input_digest:     745a7111d2b94ee3891c75f4a8a343e03230606bd3cece800b473ce522078d10   (content of crates/**, scripts/**, Cargo.toml, Cargo.lock)
profile:          debug
features:         <default>
toolchain:        rustc 1.96.0-nightly (1e2183119 2026-03-15) / cargo 1.96.0-nightly (cbb9bb8bd 2026-03-13)
host:             Darwin 24.6.0 arm64
--- building ---
build_exit:       0
binary_source:    target/debug/arc-node
binary_sha256:    429f095c6447cb7885b4d0fc8be80a24c165c12738e4d5f282b47143eaa51a47
binary_bytes:     194738680
immutable_copy:   /tmp/arc-provenance/arc-node-429f095c6447cb78   (read-only; this is what the run executes)
"""


class ProvenanceParsing(unittest.TestCase):
    def test_the_real_build_record_yields_its_binary_digest(self):
        with tempfile.NamedTemporaryFile("w", delete=False, suffix=".txt") as fh:
            fh.write(REAL_RECORD)
        try:
            rec = orchestrate.read_provenance(fh.name)
        finally:
            os.unlink(fh.name)
        self.assertEqual(
            rec["binary_sha256"],
            "429f095c6447cb7885b4d0fc8be80a24c165c12738e4d5f282b47143eaa51a47")
        self.assertEqual(rec["dirty_files"], "0")
        self.assertEqual(rec["source_revision"], "a283e9dfa7c2c1257886eb406165c305c9a15eda")
        # Trailing annotations are not part of the value.
        self.assertEqual(rec["input_digest"],
                         "745a7111d2b94ee3891c75f4a8a343e03230606bd3cece800b473ce522078d10")


class JsonlFreshness(unittest.TestCase):
    def test_a_record_file_that_already_exists_is_refused(self):
        d = tempfile.mkdtemp()
        path = os.path.join(d, "samples.jsonl")
        open(path, "w").write('{"stale": true}\n')
        with self.assertRaises(FileExistsError):
            orchestrate.JsonlWriter(path)


if __name__ == "__main__":
    unittest.main(verbosity=2)
