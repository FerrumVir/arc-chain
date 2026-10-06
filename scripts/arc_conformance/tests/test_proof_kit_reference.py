"""Tests for the independent Proof Kit reference (challenge derivation and result checks).

Run from scripts/:  python3 -m unittest arc_conformance.tests.test_proof_kit_reference -v
"""

from __future__ import annotations

import copy
import json
import unittest
from pathlib import Path

from arc_conformance import proof_kit_reference as ref

GOLDEN_REFERENCE = ref.REPO_ROOT / "docs" / "protocol" / "proof-kit" / "smollm3-golden-reference.json"
CASES_FILE = ref.REPO_ROOT / "scripts" / "arc_modern" / "smollm3_cases.json"
PINNED_MANIFEST = ref.REPO_ROOT / "docs" / "protocol" / "packages" / "smollm3-3b.integer-package.json"


def sample_result() -> dict:
    """A result built from the published reference (needs blake3 for its digests)."""
    golden = json.loads(GOLDEN_REFERENCE.read_text(encoding="utf-8"))
    cases = [{"id": c["id"], "tokens": c["tokens"], "output_hash": c["output_hash"],
              "logits_digest": c["logits_digest"]} for c in golden["cases"]]
    tokens = [791, 6864, 315, 128012]
    case = {"id": "challenge", "tokens": tokens, "output_hash": ref.tokens_hash(tokens),
            "logits_digest": "ab" * 32}
    challenge_digest = ref.matrix_digest([case])
    run = {"backend": "cpu-scalar", "isa": None, "verdict": "MATCH",
           "golden_digest": ref.PUBLISHED_GOLDEN_DIGEST, "challenge_digest": challenge_digest,
           "prefill_tok_s": 1.325, "decode_tok_s": 1.322, "threads": 4,
           "vector_projections": {"attempted": 0, "accepted": 0}, "adapter": None,
           "divergence": None}
    simd = dict(run, backend="cpu-simd", isa="avx2", prefill_tok_s=3.476, decode_tok_s=3.437,
                vector_projections={"attempted": 121187, "accepted": 121187})
    challenge = dict(ref.TEST_CHALLENGE)
    challenge.update({"prompt_sha256": ref.sha256_hex(ref.challenge_prompt(challenge["seed"])),
                      "digest": challenge_digest, "case": case})
    return {
        "schema": ref.RESULT_SCHEMA, "kit_version": "0.1.0", "arc_version": "0.8.10",
        "nonce": "0123456789abcdef0123456789abcdef", "model": dict(ref.PINNED_MODEL),
        "verdict": "MATCH",
        "golden": {"published_digest": ref.PUBLISHED_GOLDEN_DIGEST,
                   "digest": ref.matrix_digest(cases), "cases": cases},
        "challenge": challenge, "speed_method": ref.SPEED_METHOD, "runs": [run, simd],
        "device": {"os": "linux", "os_version": "ubuntu 24.04", "arch": "x86_64",
                   "cpu_model": "AMD EPYC 7763 64-Core Processor", "logical_cpus": 4,
                   "cpu_features": ["avx2", "fma"], "gpu_model": None},
        "island": {"memory_class_gb": 16, "unified_memory": False, "gpu_vram_class_gb": None,
                   "thunderbolt5": None, "download_mbps_class": 1000},
    }


class ChallengeDerivation(unittest.TestCase):
    def test_word_list_is_published(self) -> None:
        words = ref.load_words()
        self.assertEqual(len(words), 256)
        self.assertEqual((words[0], words[-1]), ("acorns", "zebras"))

    def test_vectors_file_matches_this_implementation(self) -> None:
        published = json.loads(ref.VECTORS_PATH.read_text(encoding="utf-8"))
        self.assertEqual(published["words_sha256"], ref.WORDS_SHA256)
        self.assertEqual(published["domain_hex"], ref.CHALLENGE_DOMAIN.hex())
        self.assertEqual(published, ref.make_vectors(len(published["vectors"]) - 3))
        for vector in published["vectors"]:
            prompt = ref.challenge_prompt(vector["seed"])
            self.assertEqual(prompt, vector["prompt"])
            self.assertEqual(ref.word_indices(vector["seed"]), vector["word_indices"])
            self.assertEqual(ref.sha256_hex(prompt), vector["prompt_sha256"])

    def test_test_challenge_prompt(self) -> None:
        self.assertEqual(
            ref.challenge_prompt(ref.TEST_CHALLENGE["seed"]),
            "Write one short sentence that mentions frogs, kayaks, buttons and teapots.")
        case = ref.challenge_case(ref.TEST_CHALLENGE["seed"])
        self.assertEqual((case["id"], case["max_tokens"], case["eos"], case["selection"]),
                         ("challenge", 24, [128012], "rp64-argmax"))

    def test_bad_seeds_are_refused(self) -> None:
        for bad in ["", "00", "0" * 63, "A" * 64, "g" * 64, 7]:
            with self.assertRaises(ref.ChallengeError):
                ref.challenge_prompt(bad)  # type: ignore[arg-type]

    def test_pinned_constants_match_the_repository(self) -> None:
        manifest = json.loads(PINNED_MANIFEST.read_text(encoding="utf-8"))
        self.assertEqual(manifest["package"]["sha256"], ref.PINNED_MODEL["package_sha256"])
        self.assertEqual(manifest["manifest_blake3"], ref.PINNED_MODEL["manifest_blake3"])
        self.assertEqual(manifest["source"]["revision"], ref.PINNED_MODEL["revision"])
        cases = json.loads(CASES_FILE.read_text(encoding="utf-8"))["cases"]
        self.assertEqual([(c["id"], c["max_tokens"]) for c in cases], ref.GOLDEN_CASES)


@unittest.skipUnless(ref.have_blake3(), "needs the blake3 package")
class ResultValidation(unittest.TestCase):
    def test_reference_reproduces_the_published_digest(self) -> None:
        golden = json.loads(GOLDEN_REFERENCE.read_text(encoding="utf-8"))
        self.assertEqual(ref.matrix_digest(golden["cases"]), ref.PUBLISHED_GOLDEN_DIGEST)
        for case in golden["cases"]:
            self.assertEqual(ref.tokens_hash(case["tokens"]), case["output_hash"])
            self.assertEqual(len(case["logits_hashes"]),
                             len(case["prompt_tokens"]) + len(case["tokens"]) - 1)

    def test_a_complete_result_validates(self) -> None:
        result = sample_result()
        body = json.dumps(result, indent=2).encode()
        self.assertEqual(ref.validate_result(result, raw_bytes=len(body)), [])
        self.assertLess(len(body), ref.MAX_RESULT_BYTES)

    def test_a_mismatch_needs_a_divergence(self) -> None:
        result = sample_result()
        result["runs"][1]["golden_digest"] = "cd" * 32
        result["runs"][1]["verdict"] = "MISMATCH"
        result["verdict"] = "MISMATCH"
        self.assertTrue(any("divergence" in e for e in ref.validate_result(result)))
        result["runs"][1]["divergence"] = {"case": "haiku", "position": 81, "layer": 7,
                                           "op": "w_gate"}
        self.assertEqual(ref.validate_result(result), [])

    def test_a_gpu_run_slots_in(self) -> None:
        result = sample_result()
        gpu = dict(result["runs"][1], backend="gpu-wgpu", isa=None, threads=None,
                   vector_projections=None,
                   adapter={"vendor": "nvidia", "device": "NVIDIA GeForce RTX 4070",
                            "backend": "vulkan", "driver": "560.94"})
        result["runs"].append(gpu)
        self.assertEqual(ref.validate_result(result), [])

    def test_tampered_results_are_rejected(self) -> None:
        def mutate(path, value):
            def apply(result):
                target = result
                for key in path[:-1]:
                    target = target[key]
                target[path[-1]] = value
            return apply

        mutations = {
            "hostname": mutate(["hostname"], "alice-laptop"),
            "serial": mutate(["device", "serial"], "C02X"),
            "user name in a label": mutate(["device", "cpu_model"], "Apple M2\nalice"),
            "nonce": mutate(["nonce"], "xyz"),
            "schema": mutate(["schema"], "arc.proof-result.v2"),
            "model": mutate(["model", "package_sha256"], "00" * 32),
            "claimed match": mutate(["golden", "digest"], "11" * 32),
            "run verdict": mutate(["runs", 0, "verdict"], "MISMATCH"),
            "top verdict": mutate(["verdict"], "MISMATCH"),
            "tokens": mutate(["golden", "cases", 0, "tokens", 0], 1),
            "case order": mutate(["golden", "cases", 0, "id"], "haiku"),
            "challenge prompt": mutate(["challenge", "prompt_sha256"], "22" * 32),
            "challenge digest": mutate(["challenge", "digest"], "33" * 32),
            "challenge time": mutate(["challenge", "expires_at"], "tomorrow"),
            "duplicate feature": mutate(["device", "cpu_features"], ["avx2", "avx2"]),
            "memory class": mutate(["island", "memory_class_gb"], 17),
            "adapter on a CPU run": mutate(["runs", 0, "adapter"], {
                "vendor": "x", "device": "y", "backend": "gl", "driver": "1"}),
            "negative speed": mutate(["runs", 0, "decode_tok_s"], -1),
            "boolean threads": mutate(["runs", 0, "threads"], True),
            "zero threads": mutate(["runs", 0, "threads"], 0),
        }
        good = sample_result()
        for name, apply in mutations.items():
            result = copy.deepcopy(good)
            apply(result)
            with self.subTest(name):
                self.assertNotEqual(ref.validate_result(result), [], name)
        duplicate = copy.deepcopy(good)
        duplicate["runs"].append(copy.deepcopy(duplicate["runs"][0]))
        self.assertNotEqual(ref.validate_result(duplicate), [])
        self.assertNotEqual(ref.validate_result(good, raw_bytes=ref.MAX_RESULT_BYTES + 1), [])

    def test_cli_validates_a_file(self) -> None:
        import tempfile
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "proof-kit-result.json"
            path.write_text(json.dumps(sample_result()), encoding="utf-8")
            self.assertEqual(ref.main(["validate", str(path), "--expect-verdict", "MATCH"]), 0)
            self.assertEqual(ref.main(["validate", str(path), "--expect-verdict", "MISMATCH"]), 1)


if __name__ == "__main__":
    unittest.main()
