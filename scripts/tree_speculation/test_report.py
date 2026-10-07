import copy
import hashlib
import json
import unittest
from pathlib import Path

from report import quantile, summarize


class EvidenceContract(unittest.TestCase):
    def evidence(self):
        records = [{"kind": "metadata", "pr_head": "test", "head": "merge"}]
        for traffic in ("coding", "agent", "chat"):
            for i in range(20):
                case = f"{traffic}-{i}"
                timing = dict(prefill_seconds=1., decode_seconds=2., draft_seconds=.1, verify_seconds=1.8)
                records.append(dict(kind="baseline", id=case, traffic=traffic, source_id=case,
                    full_tree_identity=i < 2, prompt_tokens=2, prompt_token_ids=[1, 2],
                    max_tokens=128, eos=[9], output_tokens=[3, 4, 9], decode_tokens=2,
                    verification_passes=2, logits_hashes=["h"] * 4,
                    output_blake3="o", kv_digest="k", timing=timing,
                    lookup_replay=dict(passes=1, nodes=4, expanded_rows=6, seconds=.1)))
                if i < 2:
                    for drafter in ("lookup", "recycle", "hybrid"):
                        records.append(dict(kind="full_tree", id=case, traffic=traffic, drafter=drafter,
                            decode_tokens=2, verification_passes=1, physical_rows=4, path_lowered_rows=6,
                            tokens_logits_kv_identical=True, output_blake3="o", kv_digest="k", timing=timing))
        return records + [dict(kind="complete")]

    def test_complete_evidence_and_quantiles(self):
        result, summary = summarize(self.evidence())
        self.assertEqual(len(result["acceptance"]), 12)
        self.assertIn("not full-tree identity", summary)
        self.assertEqual(quantile([1, 2, 4], .1), 1.2)
        self.assertEqual(quantile([1, 2, 4], .9), 3.6)

    def test_incomplete_failed_identity_eos_and_replay_fail_closed(self):
        records = self.evidence()
        bad = [records[:-1], records[:2] + records[3:]]
        for field, value in [("tokens_logits_kv_identical", False), ("physical_rows", 5)]:
            copy_records = copy.deepcopy(records)
            copy_records[2][field] = value
            bad.append(copy_records)
        for output in ([9, 3, 4], [3, 4, 5]):
            copy_records = copy.deepcopy(records)
            copy_records[1]["output_tokens"] = output
            bad.append(copy_records)
        for data in bad:
            with self.assertRaises(AssertionError):
                summarize(data)

    def test_fixture_and_license_integrity(self):
        root = Path(__file__).resolve().parents[2]
        fixture = root / "crates/arc-inference/tests/fixtures/tree_public"
        data = json.loads((fixture / "cases.json").read_text())
        for traffic in ("coding", "agent", "chat"):
            cases = [c for c in data["cases"] if c["traffic"] == traffic]
            self.assertEqual(len(cases), 20)
            self.assertEqual(sum(c["full_tree_identity"] for c in cases), 2)
            hashes = [hashlib.sha256((data["sampling"]["seed"] + c["source_id"]).encode()).hexdigest() for c in cases]
            self.assertEqual(hashes, sorted(hashes))
            self.assertEqual(hashes, [c["selection_hash"] for c in cases])
            self.assertTrue(all(c["max_tokens"] >= 128 for c in cases))
        for name, info in json.loads((fixture / "licenses/manifest.json").read_text()).items():
            self.assertEqual(hashlib.sha256((fixture / "licenses" / name).read_bytes()).hexdigest(), info["sha256"])


if __name__ == "__main__":
    unittest.main()
