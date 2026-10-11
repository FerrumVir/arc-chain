# Batch 3 model validation (2026-09-19)

The native source-pinned artifact is installed and verified:

- Path: `~/.arc/models/standard.gguf`
- Size: `4,081,004,224` bytes
- SHA-256: `08a5566d61d7cb6b420c3e4387a39e0078e1f2fe5f055f3a03887385304d4bfa`
- Download used `curl -L --continue-at - --fail --retry 2 --max-time 1800`; the file was atomically renamed only after exact size and SHA-256 checks, with no destination overwrite.

`crates/arc-inference/examples/validate_canonical_artifact.rs` is registered as a candle-only example. It requires `--model`, bounds max tokens to 1–32, hashes the artifact with BLAKE3, loads `load_cached_model_canonical_i8`, enforces complete canonical INT8, follows production template/tokenizer/BOS admission, runs two isolated generations, emits token IDs/decoded text/token-byte and executor hashes/timings, and exits nonzero on mismatch or zero output. New stderr markers identify artifact hash, model load, and each generation phase. No logits or quality claim is made.

Latest source compile check passed at `2026-09-19T23:17:47Z`:

```text
cargo check -p arc-inference --example validate_canonical_artifact --features candle
Finished `dev` profile ... in 0.55s
```

One real-model run was started with:

```text
/usr/bin/time -l cargo run -p arc-inference --example validate_canonical_artifact --features candle --release -- --model ~/.arc/models/standard.gguf --max-tokens 8
```

It ran for 558.58 seconds, including a 129-second release build, produced no JSON ([durable empty evidence](batch3-canonical-validation.json)), and was terminated by killing only harness PID 73022 after shared-host memory pressure became severe. `/usr/bin/time -l` recorded 5,440,274,432-byte maximum RSS and 8,184,027,640-byte peak footprint. Root observed shared-host swap near 16 GiB and 31% free; this does not prove the runner alone caused the pressure. No profile or generation phase evidence was observed, and no valid inference, BLAKE3 output, or quality/economic/network completion claim exists. The complete captured stderr is [batch3-canonical-validation.log](batch3-canonical-validation.log). The run predates the new stderr markers, so the log cannot identify its stalled phase; a later resource-safe run should use them.
