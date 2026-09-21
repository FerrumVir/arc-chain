"""Rebuild the committed integer-inference KAT model and recompute its answers.

The fixture recipe (tests/fixtures/integer_inference_kat.json) defines a small
synthetic model from a 64-bit LCG. This module rebuilds it and runs it through
the independent reference executor, so every committed digest can be checked
without the Rust engine.

    python3 -m arc_conformance.kat [path/to/integer_inference_kat.json]
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

import blake3

from arc_conformance import integer_reference as ref

REPO = Path(__file__).resolve().parents[2]
DEFAULT_FIXTURE = REPO / "crates/arc-inference/tests/fixtures/integer_inference_kat.json"
MASK64 = (1 << 64) - 1

# Q16 samples of the unit circle at multiples of pi/8 (fixture constants).
FIXTURE_COS = [65536, 60547, 46341, 25080, 0, -25080, -46341, -60547,
               -65536, -60547, -46341, -25080, 0, 25080, 46341, 60547]
FIXTURE_SIN = [0, 25080, 46341, 60547, 65536, 60547, 46341, 25080,
               0, -25080, -46341, -60547, -65536, -60547, -46341, -25080]


class FixtureRng:
    def __init__(self, seed: int):
        self.state = seed & MASK64

    def next_u64(self) -> int:
        self.state = (self.state * 6364136223846793005 + 1442695040888963407) & MASK64
        return self.state

    def weights(self, rows: int, cols: int):
        data = [max((self.next_u64() >> 56) - 128, -127) for _ in range(rows * cols)]
        scales = [64 + ((self.next_u64() >> 32) % 449) for _ in range(rows)]
        return data, scales

    def norm(self, length: int):
        return [ref.ONE + (self.next_u64() % 8193) - 4096 for _ in range(length)]


def fixture_rope_tables(d_head: int, max_seq: int):
    half = d_head // 2
    cos, sin = [], []
    for position in range(max_seq):
        for pair in range(half):
            angle = (position * (pair + 1)) % 16
            cos.append(FIXTURE_COS[angle])
            sin.append(FIXTURE_SIN[angle])
    return cos, sin


def blake3_hex(data: bytes) -> str:
    return blake3.blake3(data).hexdigest()


def build(fixture: dict, profile: str = ref.LEGACY_SPLIT_HALF):
    d_model, n_heads, n_kv = fixture["d_model"], fixture["n_heads"], fixture["n_kv_heads"]
    d_ff, vocab = fixture["d_ff"], fixture["vocab_size"]
    d_head = d_model // n_heads
    d_kv = d_head * n_kv
    rng = FixtureRng(int(fixture["model_seed"], 16))

    weight_stream = bytearray()

    def matrix(rows, cols):
        data, scales = rng.weights(rows, cols)
        weight_stream.extend(bytes(w & 0xFF for w in data))
        for s in scales:
            weight_stream.extend(s.to_bytes(8, "little", signed=True))
        return data, scales

    emb_data, emb_scales = matrix(vocab, d_model)
    embedding_q16 = [emb_data[r * d_model + c] * emb_scales[r]
                     for r in range(vocab) for c in range(d_model)]
    out_data, out_scales = matrix(vocab, d_model)

    layers = []
    for _ in range(fixture["n_layers"]):
        wq = matrix(d_model, d_model)
        wk = matrix(d_kv, d_model)
        wv = matrix(d_kv, d_model)
        wo = matrix(d_model, d_model)
        wg = matrix(d_ff, d_model)
        wu = matrix(d_ff, d_model)
        wd = matrix(d_model, d_ff)
        attn_norm = rng.norm(d_model)
        ffn_norm = rng.norm(d_model)
        layers.append(ref.Layer(
            ref.Matrix(*wq, d_model, d_model), ref.Matrix(*wk, d_kv, d_model),
            ref.Matrix(*wv, d_kv, d_model), ref.Matrix(*wo, d_model, d_model),
            ref.Matrix(*wg, d_ff, d_model), ref.Matrix(*wu, d_ff, d_model),
            ref.Matrix(*wd, d_model, d_ff), attn_norm, ffn_norm))
    final_norm = rng.norm(d_model)
    rope_cos, rope_sin = fixture_rope_tables(d_head, fixture["max_seq"])

    model = ref.Model(
        profile=profile, d_model=d_model, n_heads=n_heads, n_kv_heads=n_kv,
        d_ff=d_ff, vocab_size=vocab, max_seq=fixture["max_seq"],
        attn_scale=23170,  # round(2**16 / sqrt(8)); the fixture fixes d_head = 8
        rope_cos=rope_cos, rope_sin=rope_sin, embedding_q16=embedding_q16,
        layers=layers, final_norm=final_norm,
        output=ref.Matrix(out_data, out_scales, vocab, d_model), bos_token=1)
    return model, blake3_hex(bytes(weight_stream))


def cache_hash(model, cache) -> str:
    out = bytearray(cache["len"].to_bytes(8, "little"))
    for keys, values in zip(cache["k"], cache["v"]):
        flat_k = [x for row in keys for x in row]
        flat_v = [x for row in values for x in row]
        out.extend(len(flat_k).to_bytes(8, "little"))
        out.extend(ref.i64_le_bytes(flat_k))
        out.extend(len(flat_v).to_bytes(8, "little"))
        out.extend(ref.i64_le_bytes(flat_v))
    return blake3_hex(bytes(out))


def run_sequence(model, tokens):
    cache = model.new_cache()
    next_tokens, logits_hashes = [], []
    for token in tokens:
        logits = model.forward(token, cache)
        next_tokens.append(ref.argmax(logits))
        logits_hashes.append(blake3_hex(ref.i64_le_bytes(logits)))
    return {"next_tokens": next_tokens, "logits_hashes": logits_hashes,
            "kv_cache_hash": cache_hash(model, cache)}


def run_split(model, tokens, boundaries):
    first, second = boundaries
    cache = model.new_cache()
    hidden_hashes = []
    for position, token in enumerate(tokens):
        hidden = model.run_layers(model.embed(token), cache, 0, first, position)
        hidden_hashes.append(blake3_hex(ref.i64_le_bytes(hidden)))
        hidden = model.run_layers(hidden, cache, first, second, position)
        hidden_hashes.append(blake3_hex(ref.i64_le_bytes(hidden)))
        model.run_layers(hidden, cache, second, len(model.layers), position)
        cache["len"] = position + 1
    return hidden_hashes


def recompute(fixture: dict) -> dict:
    model, weight_hash = build(fixture)
    sequence = run_sequence(model, fixture["sequence_tokens"])
    generated = model.generate_v1(fixture["generation_prompt"],
                                  fixture["generation_max_tokens"], [])
    return {
        "model_weight_hash": weight_hash,
        **sequence,
        "shard_hidden_hashes": run_split(model, fixture["sequence_tokens"],
                                         fixture["shard_boundaries"]),
        "generated_tokens": generated,
        "generated_output_hash": blake3_hex(ref.tokens_le_bytes(generated)),
    }


def compare(fixture: dict) -> list[str]:
    actual = recompute(fixture)
    mismatches = []
    for key, expected in fixture["expected"].items():
        if actual.get(key) != expected:
            mismatches.append(f"{key}: expected {expected!r}, reference computed {actual.get(key)!r}")
    return mismatches


def main(argv: list[str]) -> int:
    path = Path(argv[1]) if len(argv) > 1 else DEFAULT_FIXTURE
    fixture = json.loads(path.read_text())
    mismatches = compare(fixture)
    for line in mismatches:
        print("MISMATCH", line)
    checked = len(fixture["expected"])
    print(f"{fixture['name']}: {checked - len(mismatches)}/{checked} expected fields reproduced")
    return 1 if mismatches else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
