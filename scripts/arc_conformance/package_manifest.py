"""Derive an `arc.model-package.v1` manifest from a GGUF artifact (M1).

Every field comes from the artifact's header and bytes plus the chosen
execution profile; nothing is typed in by hand. An artifact the Llama
adapter cannot execute exactly is refused rather than described:
unexpected or missing tensors, wrong shapes, RoPE scaling, odd head widths.

    python3 -m arc_conformance.package_manifest MODEL.gguf [--no-hash] [--out FILE]

See docs/protocol/model-package-contract-v1.md.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import struct
import sys
from pathlib import Path

import blake3

from arc_conformance import gguf

SCHEMA = "arc.model-package.v1"
PROFILE = "arc.gguf-llama.i8-per-row.rope-interleaved.v1"
GENERATION = "ARC-native-inference/gguf-llama-i8-interleaved-rope/generation-v2/bos-once/le-u32/v1"
TOKENIZER = "arc.gguf-llama.spm-score-merge.v1"
MAX_SEQ = 4096          # loader constant (contract §8)
NATIVE_INPUT_MAX_BYTES = 32 * 1024   # arc_types::transaction::TIER1_INPUT_BLOB_MAX
NATIVE_MAX_TOKENS = 2048             # arc_types::transaction::TIER1_MAX_TOKENS
DEFAULT_ROPE_BASE = 10000.0
RMS_EPSILON_Q16 = 1     # contract §3.3
LAYER_MATRICES = ("attn_q", "attn_k", "attn_v", "attn_output", "ffn_gate", "ffn_up", "ffn_down")
LAYER_NORMS = ("attn_norm", "ffn_norm")


class ManifestError(ValueError):
    pass


def _commit(text: str) -> str:
    return blake3.blake3(text.encode()).hexdigest()


def canonical_json(value) -> bytes:
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=True).encode()


def hash_file(path: Path) -> dict:
    b3, sha = blake3.blake3(), hashlib.sha256()
    size = 0
    with open(path, "rb") as f:
        while True:
            chunk = f.read(16 << 20)
            if not chunk:
                break
            b3.update(chunk)
            sha.update(chunk)
            size += len(chunk)
    return {"bytes": size, "blake3": b3.hexdigest(), "sha256": sha.hexdigest()}


def vocab_digest(md: dict) -> str:
    tokens = md.get("tokenizer.ggml.tokens")
    scores = md.get("tokenizer.ggml.scores")
    types = md.get("tokenizer.ggml.token_type")
    if not tokens or scores is None or types is None or not len(tokens) == len(scores) == len(types):
        raise ManifestError("tokenizer tokens/scores/token_type are missing or of different lengths")
    h = blake3.blake3()
    for token, score, kind in zip(tokens, scores, types):
        data = token.encode("utf-8")
        h.update(struct.pack("<I", len(data)) + data + struct.pack("<fi", score, kind))
    return h.hexdigest()


def inventory_digest(tensors) -> str:
    h = blake3.blake3()
    for t in sorted(tensors, key=lambda t: t.name):
        name = t.name.encode("utf-8")
        h.update(struct.pack("<I", len(name)) + name + struct.pack("<II", t.ggml_type, len(t.dims)))
        h.update(b"".join(struct.pack("<Q", d) for d in t.dims))
    return h.hexdigest()


def derive(path: Path, *, hash_bytes: bool = True) -> dict:
    header = gguf.read_header(path)
    md = header.metadata

    def meta(key, default=None, required=True):
        if key in md:
            return md[key]
        if required and default is None:
            raise ManifestError(f"required metadata {key} is absent")
        return default

    arch = meta("general.architecture")
    if arch != "llama":
        raise ManifestError(f"no adapter for architecture {arch!r}; only 'llama' is supported")
    n_layers = int(meta("llama.block_count"))
    d_model = int(meta("llama.embedding_length"))
    n_heads = int(meta("llama.attention.head_count"))
    n_kv = int(meta("llama.attention.head_count_kv", n_heads, required=False))
    d_ff = int(meta("llama.feed_forward_length"))
    if min(n_layers, d_model, n_heads, n_kv, d_ff) <= 0:
        raise ManifestError("a model dimension is zero")
    if d_model % n_heads or n_heads % n_kv:
        raise ManifestError("heads do not divide the model width / KV heads do not divide heads")
    d_head = d_model // n_heads
    if d_head % 2:
        raise ManifestError("RoPE needs an even head width")
    d_kv = d_head * n_kv
    rope_dims = md.get("llama.rope.dimension_count", d_head)
    if rope_dims != d_head:
        raise ManifestError(f"partial RoPE ({rope_dims} of {d_head}) is not in the profile")
    scaling = sorted(k for k in md if k.startswith("llama.rope.scaling"))
    if scaling:
        raise ManifestError(f"RoPE scaling is not in the profile: {scaling}")
    rope_base = float(md.get("llama.rope.freq_base", DEFAULT_ROPE_BASE))
    declared_context = int(md.get("llama.context_length", 0))

    tensors = {t.name: t for t in header.tensors}
    token_embd = tensors.get("token_embd.weight")
    if token_embd is None or len(token_embd.shape) != 2 or token_embd.shape[1] != d_model:
        raise ManifestError("token_embd.weight is missing or not [vocab, d_model]")
    vocab = token_embd.shape[0]
    expected = {"token_embd.weight": [vocab, d_model], "output_norm.weight": [d_model]}
    tied = "output.weight" not in tensors
    if not tied:
        expected["output.weight"] = [vocab, d_model]
    shapes = {"attn_q": [d_model, d_model], "attn_k": [d_kv, d_model], "attn_v": [d_kv, d_model],
              "attn_output": [d_model, d_model], "ffn_gate": [d_ff, d_model],
              "ffn_up": [d_ff, d_model], "ffn_down": [d_model, d_ff],
              "attn_norm": [d_model], "ffn_norm": [d_model]}
    for layer in range(n_layers):
        for key, shape in shapes.items():
            expected[f"blk.{layer}.{key}.weight"] = shape
    missing = sorted(set(expected) - set(tensors))
    unexpected = sorted(set(tensors) - set(expected))
    if missing:
        raise ManifestError(f"missing tensors: {missing[:5]}{'…' if len(missing) > 5 else ''}")
    if unexpected:
        raise ManifestError(f"tensors the profile would ignore: {unexpected[:5]}")
    wrong = [n for n, s in expected.items() if tensors[n].shape != s]
    if wrong:
        raise ManifestError(f"tensors with unexpected shapes: {wrong[:5]}")

    tokenizer_model = meta("tokenizer.ggml.model")
    if tokenizer_model != "llama":
        raise ManifestError(f"tokenizer model {tokenizer_model!r} has no qualified tokenizer")
    if len(md["tokenizer.ggml.tokens"]) != vocab:
        raise ManifestError("tokenizer vocabulary size differs from the embedding table")
    eos = md.get("tokenizer.ggml.eos_token_id")
    eos_ids = sorted(eos) if isinstance(eos, list) else [int(meta("tokenizer.ggml.eos_token_id"))]
    template = md.get("tokenizer.chat_template")

    type_counts = {}
    for t in header.tensors:
        type_counts[t.type_name] = type_counts.get(t.type_name, 0) + 1

    def matrix_bytes(rows, cols):
        return rows * cols + rows * 8          # INT8 data + i64 scale per row

    layer_bytes = sum(matrix_bytes(*shapes[k]) for k in LAYER_MATRICES) + 2 * d_model * 8
    memory = {
        "prepared_layer_bytes": n_layers * layer_bytes,
        "embedding_q16_bytes": vocab * d_model * 8,
        "embedding_i8_bytes": matrix_bytes(vocab, d_model),
        "output_i8_bytes": matrix_bytes(vocab, d_model),
        "rope_table_bytes": 2 * MAX_SEQ * (d_head // 2) * 8,
        "kv_bytes_per_position": 2 * n_layers * d_kv * 8,
    }
    memory["kv_bytes_at_max_seq"] = memory["kv_bytes_per_position"] * MAX_SEQ
    memory["prepared_total_bytes"] = sum(memory[k] for k in (
        "prepared_layer_bytes", "embedding_q16_bytes", "embedding_i8_bytes",
        "output_i8_bytes", "rope_table_bytes")) + d_model * 8
    # The largest request the native protocol admits: its prompt and output
    # limits, capped by the context window. A host must hold this KV cache
    # next to the prepared model to serve every admissible request.
    max_native_positions = min(MAX_SEQ, 1 + NATIVE_INPUT_MAX_BYTES // 4 + NATIVE_MAX_TOKENS)
    memory["max_native_request_positions"] = max_native_positions
    memory["kv_bytes_at_max_native_request"] = memory["kv_bytes_per_position"] * max_native_positions
    memory["resident_bytes_for_max_native_request"] = (
        memory["prepared_total_bytes"] + memory["kv_bytes_at_max_native_request"])

    manifest = {
        "schema": SCHEMA,
        "artifact": {"format": "gguf", "gguf_version": header.version,
                     "header_bytes": header.header_bytes,
                     **(hash_file(path) if hash_bytes else {"bytes": path.stat().st_size,
                                                            "blake3": None, "sha256": None})},
        "graph": {
            "architecture": "llama", "n_layers": n_layers, "d_model": d_model,
            "n_heads": n_heads, "n_kv_heads": n_kv, "d_head": d_head, "d_kv": d_kv,
            "d_ff": d_ff, "vocab_size": vocab, "norm": "rms", "activation": "silu-gated",
            "attention": "causal, grouped-query (query head h reads KV head h*n_kv/n_heads)",
            "rope": {"pairing": "interleaved", "base": rope_base,
                     "base_source": "llama.rope.freq_base" if "llama.rope.freq_base" in md
                     else "default (key absent)", "dimension_count": d_head},
            "max_seq": MAX_SEQ, "declared_context_length": declared_context,
            "positions_beyond_declared_context": max(0, MAX_SEQ - declared_context)
            if declared_context else None,
            "declared_rms_epsilon": md.get("llama.attention.layer_norm_rms_epsilon"),
            "executed_rms_epsilon_q16": RMS_EPSILON_Q16,
            "tied_embeddings": tied,
        },
        "tensors": {"count": len(header.tensors), "types": dict(sorted(type_counts.items())),
                    "inventory_blake3": inventory_digest(header.tensors)},
        "tokenizer": {
            "identity": TOKENIZER, "model": tokenizer_model, "tokens": vocab,
            "vocab_blake3": vocab_digest(md),
            "bos": int(meta("tokenizer.ggml.bos_token_id")), "eos": eos_ids,
            "unk": md.get("tokenizer.ggml.unknown_token_id"),
            "ignored_special_ids": {k.rsplit(".", 1)[-1]: md[k] for k in sorted(md)
                                    if k in ("tokenizer.ggml.eot_token_id",
                                             "tokenizer.ggml.eom_token_id",
                                             "tokenizer.ggml.padding_token_id")},
            "chat_template_blake3": _commit(template) if isinstance(template, str) else None,
        },
        "execution": {"profile": PROFILE, "profile_commitment": _commit(PROFILE),
                      "contract": "docs/protocol/integer-profile-contract-v1.md"},
        "generation": {
            "semantics": GENERATION, "commitment": _commit(GENERATION),
            "input": "little-endian u32 token ids; non-empty; no leading BOS; each < vocab_size",
            "admission": f"1 + prompt_tokens + max_tokens <= {MAX_SEQ}",
            "selection": "repetition penalty over the 64 most recent generated tokens "
                         "(x5/6 if positive, x6/5 otherwise, truncating, once per occurrence), "
                         "then argmax with ties to the lowest index",
            "stopping": {"eos": eos_ids, "eos_included_in_output": True, "max_tokens": True},
            "output": "little-endian u32 token ids; output_hash = BLAKE3 of those bytes",
        },
        "partitioning": {
            "row_partitionable": [{"tensor": k, "rows": shapes[k][0], "cols": shapes[k][1]}
                                  for k in LAYER_MATRICES]
            + [{"tensor": "output", "rows": vocab, "cols": d_model}],
            "row_collective": "concatenate output rows in row order; no reduction",
            "layer_pipeline": {"supported": True,
                               "handoff_bytes_per_token": d_model * 8,
                               "handoff": "Q16 i64 hidden state after the last layer of the range"},
            "column_partition": "not defined: it needs a cross-worker reduction whose rounding "
                                "the profile does not specify",
        },
        "memory": memory,
        "supported_outputs": ["token_ids"],
    }
    manifest["manifest_blake3"] = blake3.blake3(canonical_json(manifest)).hexdigest()
    return manifest


def verify(manifest: dict) -> None:
    body = {k: v for k, v in manifest.items() if k != "manifest_blake3"}
    if blake3.blake3(canonical_json(body)).hexdigest() != manifest.get("manifest_blake3"):
        raise ManifestError("manifest_blake3 does not match the manifest body")


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("artifact", type=Path)
    parser.add_argument("--no-hash", action="store_true",
                        help="skip hashing the artifact bytes (header-only manifest)")
    parser.add_argument("--out", type=Path)
    args = parser.parse_args(argv[1:])
    try:
        manifest = derive(args.artifact, hash_bytes=not args.no_hash)
    except (ManifestError, gguf.GgufError) as error:
        print(f"refused: {error}", file=sys.stderr)
        return 1
    text = json.dumps(manifest, indent=1, sort_keys=True) + "\n"
    if args.out:
        args.out.write_text(text)
    else:
        sys.stdout.write(text)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
