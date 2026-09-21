# ARC model package contract v1 (`arc.model-package.v1`)

Status: **defined and generated; not yet enforced by the node.**
`scripts/arc_conformance/package_manifest.py` derives the manifest and
refuses artifacts the adapter cannot execute exactly. §7 lists what the node
still has to do.

## 1. Identity layers

| Layer | Identity | Bound today by |
|---|---|---|
| Artifact | BLAKE3 of the GGUF file's bytes | native activation `model_hash`/`artifact`; `CanonicalI8NativeExecutor::load_qualified` re-hashes the file before loading |
| Execution profile | BLAKE3 of `arc.gguf-llama.i8-per-row.rope-interleaved.v1` | activation `profile_hash` ([integer contract](integer-profile-contract-v1.md) §1) |
| Generation semantics | BLAKE3 of `ARC-native-inference/gguf-llama-i8-interleaved-rope/generation-v2/bos-once/le-u32/v1` | activation `generation_hash` |
| **Package** | BLAKE3 of the manifest (§3) | **not yet bound** (§7) |

The manifest adds nothing a node could not recompute. Its value is that every
derived fact becomes one reviewable, hashable object: shapes, tokenizer,
limits, memory and partitioning. A reviewer can then approve a package by
hash, instead of approving three hashes and trusting the loader for the rest.

## 2. Derivation

A manifest is a pure function of (artifact bytes, profile, generation
semantics). Derivation reads only the GGUF header, which is bounded: strings
≤ 16 MiB, arrays ≤ 16 Mi entries, ≤ 65,536 tensors, ≤ 4 dimensions. It
then hashes the file.

**Refusals.** Derivation fails, and no manifest exists, when:

* `general.architecture` is not `llama` (the only adapter);
* heads do not divide `d_model`, KV heads do not divide heads, or the head
  width is odd;
* `llama.rope.dimension_count` ≠ head width (partial RoPE), or any
  `llama.rope.scaling.*` key is present;
* the tensor set is not exactly `token_embd`, `output` (absent means tied
  embeddings), `output_norm`, and per layer `attn_q/k/v`, `attn_output`,
  `ffn_gate/up/down`, `attn_norm`, `ffn_norm`. A missing tensor is refused, and so is an
  **extra** one, because the profile would silently ignore it (e.g. `rope_freqs`);
* any tensor's shape differs from what the metadata implies;
* the tokenizer model is not `llama`, or its vocabulary size differs from the
  embedding table.

Every refusal corresponds to a case where the integer engine would either
fail at load or compute something other than the model the file describes.
The M6 startup check applies the same architecture rule.

## 3. Manifest fields

The manifest is canonical JSON: keys sorted, no whitespace, ASCII-escaped.
`manifest_blake3` is the BLAKE3 of the canonical encoding of every other
field.

| Field | Content |
|---|---|
| `artifact` | format, GGUF version, header size, byte length, BLAKE3, SHA-256 |
| `graph` | layers, widths, heads, KV heads, FFN width, vocabulary, RMS norm, gated SiLU, grouped-query attention mapping, RoPE (pairing, base, whether the base came from metadata or the 10000 default), `max_seq` = 4096, the declared context length and any positions beyond it, the declared RMS epsilon next to the executed one (1 Q16 unit), tied embeddings |
| `tensors` | count, count per GGML type, and an inventory digest over (name, type, dims) sorted by name |
| `tokenizer` | identity `arc.gguf-llama.spm-score-merge.v1`, model, size, a vocabulary digest over (token bytes, score, type) in id order, BOS/EOS/UNK ids, special ids the engine **ignores** (EOT/EOM/padding, if present), chat-template digest or null |
| `execution` | profile and its commitment; the integer contract |
| `generation` | semantics and commitment; input rules, admission bound, selection rule, stopping (EOS ids, EOS included, `max_tokens`), output encoding and hash |
| `partitioning` | the row-partitionable projections with their dimensions, the collective for a row partition (concatenate in row order, no reduction), layer pipelining and its per-token hand-off (8·`d_model` bytes). Column partitioning is **not defined**: it needs a cross-worker reduction whose rounding the profile does not specify |
| `memory` | prepared layer bytes, Q16 embedding, INT8 embedding and output, RoPE tables, KV bytes per position, at `max_seq`, and at the largest admissible native request; the resident total for that request |
| `supported_outputs` | `token_ids` only. Logits and embeddings are not paid outputs |

## 4. The canonical artifact

`/Users/excaulibur/.arc/models/standard.gguf` (LLaMA v2 7B, GGUF v2):

| Fact | Value |
|---|---|
| bytes | 4,081,004,224 |
| BLAKE3 | `934efc12a2ed8372a944e5aaedf059a8a0f42c0906f6b2f1fb3626bdeb1ffa67` |
| SHA-256 | `08a5566d61d7cb6b420c3e4387a39e0078e1f2fe5f055f3a03887385304d4bfa` (matches the truncated `08a5566d…` in `completion-ledger.md`) |
| shape | 32 layers, d 4096, 32 heads (32 KV), head 128, FFN 11008, vocabulary 32000 |
| RoPE base | 10000.0 (default (key absent)) |
| context | declared 4096, executed 4096 |
| RMS epsilon | declared 1e-06, executed 1 Q16 unit (≈1.5·10⁻⁵) |
| tensors | 291: F32 65, Q4_K 193, Q6_K 33 |
| tokenizer | 32000 tokens, BOS 1, EOS [2], UNK 0, no chat template, no ignored special ids; vocabulary digest `554ac0fc5b0a6951…` |
| prepared model | 7.80 GB (layers 6.49 GB, Q16 embedding 1.05 GB) |
| KV cache | 2,097,152 bytes per position; 8.59 GB at the largest native request (4096 positions) |
| resident at the largest request | **16.39 GB** |
| manifest | `fecaf64104b3be988ff5f3ddf3c38e6739787f9cd390b64835dc59aecb0309bf` ([`packages/llama-2-7b-q4km.manifest.json`](packages/llama-2-7b-q4km.manifest.json)) |

## 5. Finding: the largest admissible request does not fit a 16 GB host

A native request may carry 32 KiB of prompt (8,192 ids) and ask for 2,048
tokens. The 4,096-position context caps it, so the largest admissible request
needs 4,096 positions. At 2 MiB of KV per position that is **8 GiB of KV
cache** beside 7.8 GB of prepared model, which exceeds this 16 GB host.
Nothing in the node reserves or bounds that memory before execution. A
validator would swap or be killed mid-request. Honest validators with more
memory would still certify, so this is a liveness and cost problem, not a
safety one. The request's refund at expiry is not affected.

This needs an owner decision: lower the paid-request position limit for this
package, or state a minimum validator memory. Either would go in the manifest.
Until then, a node-side guard that refuses (does not vote on) a job whose KV
would exceed a configured budget is the safe default. Refusal leads to expiry
and refund. That guard is not yet written (R5).

## 6. Versioning

* Any change to the artifact's bytes is a new package.
* A change of profile or generation semantics is a new identity string, hence
  a new commitment and a new package.
* A change to this schema (a field added, removed or re-derived) is a new
  schema id (`arc.model-package.v2`), never a silent extension.
* The manifest never carries hand-entered values. If a fact cannot be derived,
  it belongs in the profile or generation contract, not here.

## 7. Not yet implemented

1. The node derives the manifest at load and refuses a mismatch with a pinned
   manifest hash (Rust, beside `load_qualified`).
2. The native activation binds the manifest hash alongside the three it binds
   today.
3. Tokenizer qualification against a pinned reference (M3). The vocabulary
   digest identifies the vocabulary; it does not show that ARC encodes text
   the way the reference does.
4. The memory guard in §5.
