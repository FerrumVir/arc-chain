# Receipts

> **Historical evidence catalog — not current production status.** Several URLs,
> topology claims, and verification labels below predate the 2026-08-26 audit.
> The public fleet is forked and version-skewed, community completed work was
> zero in that read-only snapshot, and v0.7.12 is not published or deployed.
> A matching public-v2 hash does not prove exact model bytes or payment. Use
> [`PRODUCTION-RECOVERY-AUDIT-2026-08-26.md`](PRODUCTION-RECOVERY-AUDIT-2026-08-26.md)
> for the current evidence boundary and re-run every receipt before quoting it.

Every claim I make about this project, with a link you can check yourself and a
command you can run. If something here is wrong, open an issue and I'll fix it
in public.

---

## The short version

**StarkWare has raised over a quarter of a billion dollars at an $8 billion
valuation. Lagrange is backed by Founders Fund and Peter Thiel. The Ethereum
Foundation has more cryptographers than I will ever meet.**

**I built this on one computer, in a spare room, in Oklahoma.**

I am not claiming I am better at cryptography than any of them. On one specific
thing — proving AI computations with zero-knowledge proofs — Lagrange is
genuinely ahead of me and I say so below.

What I am claiming is two things, both checkable:

1. **Post-quantum security is live here and it is not live anywhere else that
   matters.** Ethereum's plan for this ends around 2029. Mine is running today.
2. **The ratio.** They have hundreds of millions of dollars and teams of PhDs.
   I have a Mac Studio. Compare what's shipped, then compare what it cost.

---

## Why this matters at all

Right now, when you ask any AI a question, you cannot check the answer. You
don't know which model actually ran. You don't know if it was the one you paid
for, a cheaper one, a cached response, or something quietly truncated. You are
trusting a logo.

That is not a conspiracy — it's a technical fact, and it applies to the
companies too. **They can't prove it either.** Run the same model on two
different computers and you get two slightly different answers, because
floating-point arithmetic rounds differently depending on the chip and the
order it adds things up. There is no fingerprint to compare against.

So the entire industry went one direction: make cryptographic proofs cheap
enough that you can prove the AI ran correctly. That is extremely hard and
extremely expensive.

**I went the other direction: make the answer reproducible in the first place.**

The engine here runs on whole numbers instead of decimals. Integer arithmetic
gives the same result no matter what chip runs it or what order it adds things
up. Same model, same question, same 32-byte fingerprint — on an ARM Mac, on an
x86 Linux box, on a GPU. Once two computers can't disagree, you don't need the
expensive proof. You re-run it and compare a hash.

That's the whole idea. Everything else in this repo is implementation.

---

## Go as deep as you want

### Layer 1 — sixty seconds, no background needed

AI answers can't be checked today. I rebuilt the engine so the same question
always produces the exact same answer on any computer, which means anyone can
re-run it and confirm. Then I tried to forge a fake answer four different ways
and it caught all four.

Run it yourself — this one needs nothing but the repo and an internet
connection, and it checks a real answer against the live network:

```bash
git clone https://github.com/FerrumVir/arc-chain
cd arc-chain
bash scripts/arc-verify.sh --latest
```

### Layer 2 — five minutes, some technical background

- **Why floats break determinism:** floating-point addition isn't associative.
  `(a + b) + c` can differ from `a + (b + c)` in the last bits. Different CPUs
  use different SIMD reduction orders, so they accumulate differently. Integers
  are associative, so the problem disappears.
- **Quantization:** weights are stored INT16 — 32,767 levels per weight, 258×
  finer than INT8, at the same 2 bytes per parameter as FP16. Quality without
  floats.
- **Verification:** every inference writes an `InferenceAttestation` (tx type
  `0x16`) on-chain with the model hash, input hash and output hash. Any
  validator can re-run it and vote, exactly the way they vote on transactions.
- **Proofs:** for light clients that can't re-run a model, there's a real Circle
  STARK prover (StarkWare's Stwo, over the Mersenne-31 field) that proves dense
  layer computations at Llama-2-7B dimensions.

Start here: [`ARCHITECTURE.md`](../ARCHITECTURE.md) and
[`INFERENCE_DETERMINISM.md`](../INFERENCE_DETERMINISM.md).

### Layer 3 — read the code, try to break it

- Determinism engine — `crates/arc-inference/src/cached_integer_model.rs`
- Signatures, all four schemes — `crates/arc-crypto/src/signature.rs`
- The STARK circuit for a dense layer — `crates/arc-crypto/src/stwo_air.rs`
- The block-state circuit — same file, `ArcBlockWitnessEval`
- Falcon EVM precompile at address `0x08` — `crates/arc-vm/src/precompiles.rs`

The adversarial tests are the point. Both of these try to sneak a lie past the
circuits and both must fail:

```bash
cargo run --release --example soundness_check      --features stwo-prover
cargo run --release --example block_witness_check  --features stwo-prover
```

Requires the pinned nightly in `rust-toolchain.toml`. If you find a forgery
that gets through, tell me and I'll say so publicly.

---

## The comparison, with sources

| | Them | Here |
|---|---|---|
| **Post-quantum signatures live** | Ethereum: nothing live, roadmap ~2029 ([source](https://ethereum.org/roadmap/security/quantum-resistance/)) | ML-DSA-65 + Falcon-512 as first-class transaction signatures, today |
| **Post-quantum on a major L1** | Algorand ships native PQ accounts Q3 2026 ([source](https://algorand.co/technology/post-quantum)) | Shipped |
| **PQ signature in an EVM precompile** | Ethereum's EIP-8052 is still a proposal | `falcon512_verify` at `0x08`, live |
| **zkML — proving AI inference** | **Lagrange DeepProve is ahead of me.** Full LLM inference in production, 12M+ proofs ([source](https://lagrange.dev/blog/deepprove-1)) | Dense layers only, at 7B dimensions |
| **Capital raised** | StarkWare: $273M across six rounds, $8B valuation ([breakdown](https://en.wikipedia.org/wiki/StarkWare)) · Lagrange: $17.2M, led by Founders Fund ([their announcement](https://lagrange.dev/blog/lagrange-labs-announces-13-2m-in-seed-funding-to-revolutionize-big-data-applications-with-its-zk-coprocessing-technology)) | One Mac Studio |

StarkWare's rounds, itemised: $6M seed · $30M Series A (2018) · $75M Series B
plus $12M from the Ethereum Foundation (2021) · $50M Series C at a $2B valuation
(2021) · $100M Series D at an $8B valuation (2022).

Standards references: [FIPS 204 (ML-DSA)](https://csrc.nist.gov/pubs/fips/204/final)
· [FIPS 206 (FN-DSA / Falcon), still in draft](https://csrc.nist.gov/projects/post-quantum-cryptography)
· [StarkWare Stwo prover](https://github.com/starkware-libs/stwo)

---

## Every number, and how to reproduce it

Measured 2026-08-21 on an Apple M2 Ultra (24 cores, 64 GB), release builds.

### Signature schemes

`cargo run --release -p arc-crypto --example pq_bench`

| Scheme | Sign | Verify | Verify/sec | Bytes |
|---|---|---|---|---|
| Ed25519 | 13.2 µs | 30.1 µs | 33,278 | 96 |
| secp256k1 | 34.6 µs | 112.9 µs | 8,854 | 65 |
| ML-DSA-65 (post-quantum) | 345.8 µs | 103.8 µs | 9,635 | 5,261 |
| **Falcon-512 (post-quantum)** | 143.1 µs | **20.9 µs** | **47,852** | 1,551 |

Verify timing includes address derivation — this is the same `Signature::verify`
path the mempool uses, not a microbenchmark of the raw primitive. Numbers are
the fastest of seven batches after a warm-up, because a single run on a 24-core
desktop picks up tens of microseconds of scheduler noise. Three consecutive
runs reproduce these to within 0.2 µs.

**Falcon-512 verifies faster than Ed25519** — 20.9 µs against 30.1 µs, about
1.4× faster than the classical signature it replaces. Everyone assumes
post-quantum means slow and heavy. On Apple silicon it's the opposite.

### STARK proving scale

`cargo run --release --example stark_scale --features stwo-prover`

`cargo run --release --example air_shootout --features stwo-prover`

The circuit packs 32 multiply-accumulates into each trace row. The original
layout used one per row, which made a 2²⁴-row trace for a 7B projection — a
tall, thin shape that is close to the worst thing you can hand an FFT.

| Layer | Multiply-accumulates | Rows (1/row) | Rows (32/row) | Was | Now | Speedup |
|---|---|---|---|---|---|---|
| 256 × 1024 | 262K | 2¹⁸ | 2¹³ | 446 ms | 33 ms | 13.5× |
| 512 × 2048 | 1.0M | 2²⁰ | 2¹⁵ | 1,758 ms | 143 ms | 12.3× |
| 1024 × 4096 | 4.2M | 2²² | 2¹⁷ | 7,174 ms | 615 ms | 11.7× |
| **4096 × 4096** | **16.8M** | **2²⁴** | **2¹⁹** | **29,336 ms** | **2,564 ms** | **11.4×** |

The last row is a full Llama-2-7B attention projection proved as a single
Circle STARK in **2.6 seconds on a desktop** — about 6.5 million
multiply-accumulates proved per second.

The packed circuit proves the same statement and rejects the same forgeries;
`air_shootout` runs the adversarial checks against it every time.

**The prover is StarkWare's Stwo.** I'm not competing with it — I'm using it,
and it is far from saturated. Stwo is published at hundreds of millions of
trace cells per second on Poseidon workloads; this circuit extracts about 20
million. The remaining gap is mine to close, not theirs.

### Full 7B layer suite

`cargo run --release --example prove_7b_layers --features stwo-prover`

90 proofs, 90 real Circle STARKs, 971 s, every layer byte-identical across
three independent runs.

### Verify a real inference yourself — end to end

This is the one to run. It pulls a real attestation off the live network,
re-runs the exact same input through the sharded pipeline, and compares the
hashes. No arguments, no setup, no node of your own:

```bash
bash scripts/arc-verify.sh --latest
```

```
[1/3] Fetching inference details...
[ OK] Found attestation
       input:        '[INST] What is a blockchain? [/INST]'
       output:       '  A blockchain is a decentralized, digital ledger technology that'
       output_hash:  0x663882ae56035be5376ead6ef557bf0d0647c0e5dce56f01c78b5b6ffe55fe47
       model_hash:   0xabec2d582beb97a876c21d7ccc5e8e4833e8fd34aee0cb5b64e9f14f5ea57fdb
       engine:       INT16 integer (per-row, cross-platform deterministic) sharded pipeline

[2/3] Re-running the same input through the coordinator...
[ OK] Re-run complete
       new output_hash: 0x663882ae56035be5376ead6ef557bf0d0647c0e5dce56f01c78b5b6ffe55fe47
       new model_hash:  0xabec2d582beb97a876c21d7ccc5e8e4833e8fd34aee0cb5b64e9f14f5ea57fdb

[3/3] Comparing hashes...
  ✓ VERIFIED - both output_hash and model_hash match the attestation.
```

That's the whole thesis in one command. The answer was produced days ago by a
pipeline spread across six continents; re-running it now on demand reproduces
the identical hash. Try that with any hosted AI.

You can also verify a specific transaction: `bash scripts/arc-verify.sh <tx_hash>`.

### Live network

- 8 seed validators across 6 continents (2 currently down — check the dashboard)
- Live dashboard: http://140.82.16.112:3200
- Block explorer with on-chain inference: `explorer/index-live.html`

---

## What I am NOT claiming

I'd rather say these myself than have someone find them.

- **I did not invent post-quantum cryptography.** ML-DSA and Falcon are NIST
  standards and I use the reference implementations, on purpose — rolling your
  own post-quantum crypto is how you get owned. What's mine is the integration:
  address derivation, batch verification, the EVM precompile, and making them
  first-class transaction types rather than a library sitting in a corner.
- **I am behind Lagrange on zkML, and it isn't close.** DeepProve proves full
  LLM inference in production and has for a year. I prove dense layers. Matrix
  multiplication is also the *easy* part — the hard part is softmax and the
  normalization layers, and I haven't proved those yet.
- **My first STARK circuit was unsound.** It had four constraints, two of which
  did nothing, and it would have accepted a forged output. I found it, fixed it,
  and `soundness_check` exists so you don't have to take my word for it.
- **The circuit proves the relation modulo 2³¹−1**, without range checks, so a
  value that wraps the field isn't caught yet. Fixing that needs a lookup
  argument. It's not done.
- **The 152-byte receipt is a fingerprint, not a succinct proof.** Checking it
  independently means re-proving, not verifying. That's a deliberate tradeoff
  given the computation is deterministic, but it is not the same thing as a
  succinct proof and I won't pretend it is.

---

*If any of this is wrong, [open an issue](https://github.com/FerrumVir/arc-chain/issues).
I'd rather be corrected in public than be wrong in private.*
