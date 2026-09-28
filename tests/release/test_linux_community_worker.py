from __future__ import annotations

import importlib.util
import unittest
from pathlib import Path


SCRIPT = Path(__file__).parents[2] / "scripts/release/linux-community-worker.py"
SPEC = importlib.util.spec_from_file_location("linux_community_worker", SCRIPT)
assert SPEC and SPEC.loader
WORKER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(WORKER)


class WorkerReadinessTests(unittest.TestCase):
    def test_arc_cli_verify_keyfile_hex_is_normalized_for_worker_scoreboard(self) -> None:
        source = Path(__file__).parents[2] / "crates/arc-cli/src/keygen.rs"
        self.assertIn("println!(\"{}\", keypair.address().to_hex());", source.read_text())
        hash_source = Path(__file__).parents[2] / "crates/arc-crypto/src/hash.rs"
        self.assertIn("hex::encode(self.0)", hash_source.read_text())
        cli_address = "a" * 64
        self.assertEqual(WORKER.public_key_to_worker_id(cli_address), "0x" + cli_address)
        with self.assertRaises(RuntimeError):
            WORKER.public_key_to_worker_id("0x" + cli_address)

    def test_busy_worker_remains_visible_without_claiming_dispatch_ready(self) -> None:
        worker_id = "0x" + "a" * 64
        row = {
            "worker_id": worker_id,
            "model_id": "0x" + "b" * 64,
            "execution_profile": "INT8 integer (per-row, cross-platform deterministic)",
            "capabilities": ["inference"],
        }
        readiness = {
            "model_id": row["model_id"],
            "required_community_execution_profile": row["execution_profile"],
            "community_dispatch_ready": False,
            "live_community_workers": 0,
        }
        self.assertTrue(WORKER.exact_worker_visible(row, readiness, worker_id))
        self.assertFalse(readiness["community_dispatch_ready"])


if __name__ == "__main__":
    unittest.main()
