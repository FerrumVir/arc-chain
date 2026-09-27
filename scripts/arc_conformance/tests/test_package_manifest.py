import json
import tempfile
import unittest
from pathlib import Path

from arc_conformance import gguf, package_manifest as pm


def tiny_llama(path, *, n_layers=1, d=8, heads=2, kv=1, d_ff=16, vocab=12,
               drop=None, extra=(), metadata_extra=()):
    d_kv = d // heads * kv
    shapes = {"token_embd.weight": [vocab, d], "output.weight": [vocab, d],
              "output_norm.weight": [d]}
    for layer in range(n_layers):
        p = f"blk.{layer}"
        shapes.update({f"{p}.attn_q.weight": [d, d], f"{p}.attn_k.weight": [d_kv, d],
                       f"{p}.attn_v.weight": [d_kv, d], f"{p}.attn_output.weight": [d, d],
                       f"{p}.ffn_gate.weight": [d_ff, d], f"{p}.ffn_up.weight": [d_ff, d],
                       f"{p}.ffn_down.weight": [d, d_ff], f"{p}.attn_norm.weight": [d],
                       f"{p}.ffn_norm.weight": [d]})
    for name, shape in extra:
        shapes[name] = shape
    tensors = []
    for name, shape in shapes.items():
        if name == drop:
            continue
        count = 1
        for dim in shape:
            count *= dim
        tensors.append((name, shape, [0.25] * count))
    metadata = [
        ("general.architecture", gguf.STRING, "llama"),
        ("llama.block_count", gguf.U32, n_layers),
        ("llama.embedding_length", gguf.U32, d),
        ("llama.attention.head_count", gguf.U32, heads),
        ("llama.attention.head_count_kv", gguf.U32, kv),
        ("llama.feed_forward_length", gguf.U32, d_ff),
        ("llama.context_length", gguf.U32, 4096),
        ("tokenizer.ggml.model", gguf.STRING, "llama"),
        ("tokenizer.ggml.tokens", gguf.ARRAY, [f"t{i}" for i in range(vocab)], gguf.STRING),
        ("tokenizer.ggml.scores", gguf.ARRAY, [float(-i) for i in range(vocab)], gguf.F32),
        ("tokenizer.ggml.token_type", gguf.ARRAY, [1] * vocab, gguf.I32),
        ("tokenizer.ggml.bos_token_id", gguf.U32, 1),
        ("tokenizer.ggml.eos_token_id", gguf.U32, 2),
        *metadata_extra,
    ]
    gguf.write(path, metadata, tensors)


class Manifest(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.path = Path(self.dir.name) / "tiny.gguf"

    def tearDown(self):
        self.dir.cleanup()

    def test_a_valid_artifact_yields_a_self_verifying_deterministic_manifest(self):
        tiny_llama(self.path)
        first = pm.derive(self.path)
        second = pm.derive(self.path)
        self.assertEqual(first, second)
        pm.verify(first)
        self.assertEqual(first["graph"]["d_kv"], 4)
        self.assertEqual(first["memory"]["kv_bytes_per_position"], 2 * 1 * 4 * 8)
        self.assertEqual(first["artifact"]["bytes"], self.path.stat().st_size)
        self.assertEqual(first["execution"]["profile_commitment"],
                         "8e1c69d4b325699481e0308bde3394efe1d81ea25057471c35647d14ebff689e")
        tampered = json.loads(json.dumps(first))
        tampered["graph"]["n_layers"] = 2
        with self.assertRaises(pm.ManifestError):
            pm.verify(tampered)

    def test_any_byte_change_changes_the_identity(self):
        tiny_llama(self.path)
        before = pm.derive(self.path)
        data = bytearray(self.path.read_bytes())
        data[-1] ^= 1
        self.path.write_bytes(bytes(data))
        after = pm.derive(self.path)
        self.assertNotEqual(before["artifact"]["blake3"], after["artifact"]["blake3"])
        self.assertNotEqual(before["manifest_blake3"], after["manifest_blake3"])

    def test_artifacts_the_adapter_cannot_execute_exactly_are_refused(self):
        cases = {
            "missing norm": dict(drop="blk.0.ffn_norm.weight"),
            "ignored tensor": dict(extra=[("rope_freqs.weight", [4])]),
            "rope scaling": dict(metadata_extra=[("llama.rope.scaling.type", gguf.STRING, "linear")]),
            "odd head width": dict(d=6, heads=2, kv=1, d_ff=12),
            "partial rope": dict(metadata_extra=[("llama.rope.dimension_count", gguf.U32, 2)]),
        }
        for label, kwargs in cases.items():
            with self.subTest(label):
                tiny_llama(self.path, **kwargs)
                with self.assertRaises(pm.ManifestError):
                    pm.derive(self.path)

    def test_the_header_reader_bounds_what_it_will_parse(self):
        self.path.write_bytes(b"GGUF" + (3).to_bytes(4, "little") + (1 << 40).to_bytes(8, "little")
                              + (0).to_bytes(8, "little"))
        with self.assertRaises(gguf.GgufError):
            gguf.read_header(self.path)


if __name__ == "__main__":
    unittest.main()
