# Plan: switching the network's canonical model to SmolLM3-3B

Status: **proposal, not started.** The pull request that adds SmolLM3-3B
(`arc.hf-llama.i8-dyadic-row.q16.v1`) deliberately changes no consensus,
reward, coordinator, validator or desktop binding. This page lists what would
have to change, in order, before the network could serve SmolLM3-3B instead
of Llama-2-7B. Each step needs an owner and an explicit go decision.

## What exists after the add-model PR

- The integer engine for SmolLM3-3B: `arc_inference::modern` and the
  `arc-modern` CLI (convert, verify, generate, golden, ppl, tokenize).
- A deterministic on-device converter: pinned BF16 source
  (`docs/protocol/packages/smollm3-3b.source.json`) to a package whose SHA-256
  is identical on every OS. The pinned package manifest is
  `docs/protocol/packages/smollm3-3b.integer-package.json`, committed once CI
  has produced the same hash on all four runner types.
- CI evidence: a hash matrix (ubuntu, windows, macOS arm64, macOS x86-64,
  scalar and SIMD), golden digests from the independent Python executor,
  tokenizer and chat-template parity, and perplexity against BF16.

## Steps to switch

1. **Pin the identities.** Add the profile, generation semantics and package
   identities to `crates/arc-types/src/transaction.rs` next to
   `GGUF_LLAMA_I8_INTERLEAVED_ROPE_PROFILE_V1`. Add commitments next to
   `canonical_i8_profile_commitment()` and
   `canonical_i8_generation_commitment()` in
   `crates/arc-node/src/native_inference.rs`. Commit the package manifest hash
   in the real-execution qualification record (`package_manifest_hash`).
2. **Teach the package check the new format.** `model_package::verify_loaded_package`
   requires `artifact.format = gguf`. Either extend it to
   `arc.integer-package-manifest.v1` or call
   `modern::package::verify_package` for the new profile. Refuse to serve
   unless the converted package's SHA-256 equals the pinned manifest.
3. **Executor.** Add a `ModernModel` executor beside `CanonicalI8NativeExecutor`
   (`load_qualified`): load the verified package, admit
   `prompt + max_tokens <= 4096`, run generation `arc.hf-chat.no-bos.rp64-argmax.le-u32.v1`.
   The KV cache is 147,456 bytes per position (604 MB at 4,096 positions), so
   the `--native-kv-budget-bytes` default fits.
4. **Validators.** Re-execution needs the same engine, either whole-model or
   with row-partitioned projections. The dyadic epilogue is per row, so row
   partitioning stays exact. The `tensor_parallel` / `row_service` paths
   currently take `CachedIntegerModel` and need a `DyadicMatrix` variant.
   Validator fleet memory: 3.1 GB package plus KV.
5. **Coordinator.** Dispatch by the new profile and generation commitments.
   Workers advertise the package SHA-256; coordinators route only to workers
   whose admission check passed.
6. **Node and desktop download.** Replace the single Llama-2 GGUF source in
   `DEFAULT_MODEL_SOURCES` (`crates/arc-node/src/main.rs`) and `MODEL_TIERS`
   (`desktop/src-tauri/src/commands.rs`, `desktop/src/lib/tauri.ts`) with the
   source manifest:
   - download the two pinned BF16 shards and `config.json` (6.15 GB), resumable;
   - verify each file's SHA-256;
   - run the converter (minutes, about 1.5 GB peak RAM);
   - verify the package hash, then delete the BF16 files (3.1 GB kept).

   Show progress for conversion as well as download.
7. **Admission KAT.** On join and after every update, each node runs a short
   SmolLM3 known-answer test (one golden prompt, pinned digest). It reports
   the result and its kernel census, and does not serve if the digest
   differs.
8. **Activation.** A new native-inference activation record carries the new
   commitments. The old Llama-2 activation stays valid until its sunset height,
   so in-flight requests settle under the identity they were made with.

## Gates before step 8

| Gate | Evidence |
|---|---|
| Cross-platform hash matrix | identical package hash and golden digests on all four CI runner types, scalar and SIMD (this PR's workflow) |
| Independent reference | Python executor re-derives every golden logits vector |
| Device classes that will serve | the same digests on M1–M4, AVX2-only and AVX-512 x86, and Windows-on-ARM if admitted (self-hosted or volunteer runs) |
| Quality | perplexity delta vs BF16 within the threshold agreed before the run; task spot checks |
| Memory | conversion and serving fit 8 GB Macs with the node running |
| Rollback | the Llama-2 identity is untouched; switching back is a coordinator routing change |

## Open decisions

- Whether ARC also hosts the converted package for slow links. That is
  optional: the hash is the same either way.
- The served context. 4,096 positions is the current cap; YaRN extension
  would be a new identity.
- Whether paid requests may choose `/think` mode, which produces longer
  outputs. The default is `/no_think`.
